use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::BufReader;
use std::{cmp, io};

use obkv::KvReaderU16;

use super::helpers::{
    create_sorter, create_writer, merge_deladd_cbo_roaring_bitmaps, try_split_array_at,
    writer_into_reader, GrenadParameters, MergeFn,
};
use crate::error::SerializationError;
use crate::index::db_name::DOCID_WORD_POSITIONS;
use crate::proximity::{index_proximity, MAX_DISTANCE};
use crate::update::del_add::{DelAdd, KvReaderDelAdd, KvWriterDelAdd};
use crate::{DocumentId, Result};

/// Extracts the best proximity between pairs of words and the documents ids where this pair appear.
///
/// Returns a grenad reader with the list of extracted word pairs proximities and
/// documents ids from the given chunk of docid word positions.
#[logging_timer::time]
pub fn extract_word_pair_proximity_docids<R: io::Read + io::Seek>(
    docid_word_positions: grenad::Reader<R>,
    indexer: GrenadParameters,
) -> Result<grenad::Reader<BufReader<File>>> {
    puffin::profile_function!();

    let max_memory = indexer.max_memory_by_thread();

    let mut word_pair_proximity_docids_sorters: Vec<_> = (1..MAX_DISTANCE)
        .map(|_| {
            create_sorter(
                grenad::SortAlgorithm::Unstable,
                merge_deladd_cbo_roaring_bitmaps,
                indexer.chunk_compression_type,
                indexer.chunk_compression_level,
                indexer.max_nb_chunks,
                max_memory.map(|m| m / MAX_DISTANCE as usize),
            )
        })
        .collect();

    let mut del_word_positions: VecDeque<(String, u16)> =
        VecDeque::with_capacity(MAX_DISTANCE as usize);
    let mut add_word_positions: VecDeque<(String, u16)> =
        VecDeque::with_capacity(MAX_DISTANCE as usize);
    let mut del_word_pair_proximity = BTreeMap::new();
    let mut add_word_pair_proximity = BTreeMap::new();
    let mut current_document_id = None;

    let mut cursor = docid_word_positions.into_cursor()?;
    while let Some((key, value)) = cursor.move_on_next()? {
        let (document_id_bytes, _fid_bytes) = try_split_array_at(key)
            .ok_or(SerializationError::Decoding { db_name: Some(DOCID_WORD_POSITIONS) })?;
        let document_id = u32::from_be_bytes(document_id_bytes);

        // if we change document, we fill the sorter
        if current_document_id.map_or(false, |id| id != document_id) {
            puffin::profile_scope!("Document into sorter");

            document_word_positions_into_sorter(
                current_document_id.unwrap(),
                &del_word_pair_proximity,
                &add_word_pair_proximity,
                &mut word_pair_proximity_docids_sorters,
            )?;
            del_word_pair_proximity.clear();
            add_word_pair_proximity.clear();
        }

        current_document_id = Some(document_id);

        let (del, add): (Result<_>, Result<_>) = rayon::join(
            || {
                // deletions
                if let Some(deletion) = KvReaderDelAdd::new(value).get(DelAdd::Deletion) {
                    for (position, word) in KvReaderU16::new(deletion).iter() {
                        // drain the proximity window until the head word is considered close to the word we are inserting.
                        while del_word_positions.get(0).map_or(false, |(_w, p)| {
                            index_proximity(*p as u32, position as u32) >= MAX_DISTANCE
                        }) {
                            word_positions_into_word_pair_proximity(
                                &mut del_word_positions,
                                &mut del_word_pair_proximity,
                            )?;
                        }

                        // insert the new word.
                        let word = std::str::from_utf8(word)?;
                        del_word_positions.push_back((word.to_string(), position));
                    }

                    while !del_word_positions.is_empty() {
                        word_positions_into_word_pair_proximity(
                            &mut del_word_positions,
                            &mut del_word_pair_proximity,
                        )?;
                    }
                }

                Ok(())
            },
            || {
                // additions
                if let Some(addition) = KvReaderDelAdd::new(value).get(DelAdd::Addition) {
                    for (position, word) in KvReaderU16::new(addition).iter() {
                        // drain the proximity window until the head word is considered close to the word we are inserting.
                        while add_word_positions.get(0).map_or(false, |(_w, p)| {
                            index_proximity(*p as u32, position as u32) >= MAX_DISTANCE
                        }) {
                            word_positions_into_word_pair_proximity(
                                &mut add_word_positions,
                                &mut add_word_pair_proximity,
                            )?;
                        }

                        // insert the new word.
                        let word = std::str::from_utf8(word)?;
                        add_word_positions.push_back((word.to_string(), position));
                    }

                    while !add_word_positions.is_empty() {
                        word_positions_into_word_pair_proximity(
                            &mut add_word_positions,
                            &mut add_word_pair_proximity,
                        )?;
                    }
                }

                Ok(())
            },
        );

        del?;
        add?;
    }

    if let Some(document_id) = current_document_id {
        puffin::profile_scope!("Final document into sorter");
        document_word_positions_into_sorter(
            document_id,
            &del_word_pair_proximity,
            &add_word_pair_proximity,
            &mut word_pair_proximity_docids_sorters,
        )?;
    }
    {
        puffin::profile_scope!("sorter_into_reader");
        let mut writer = create_writer(
            indexer.chunk_compression_type,
            indexer.chunk_compression_level,
            tempfile::tempfile()?,
        );

        for sorter in word_pair_proximity_docids_sorters {
            sorter.write_into_stream_writer(&mut writer)?;
        }

        writer_into_reader(writer)
    }
}

/// Fills the list of all pairs of words with the shortest proximity between 1 and 7 inclusive.
///
/// This list is used by the engine to calculate the documents containing words that are
/// close to each other.
fn document_word_positions_into_sorter(
    document_id: DocumentId,
    del_word_pair_proximity: &BTreeMap<(String, String), u8>,
    add_word_pair_proximity: &BTreeMap<(String, String), u8>,
    word_pair_proximity_docids_sorters: &mut [grenad::Sorter<MergeFn>],
) -> Result<()> {
    use itertools::merge_join_by;
    use itertools::EitherOrBoth::{Both, Left, Right};

    let mut buffer = Vec::new();
    let mut key_buffer = Vec::new();
    for eob in
        merge_join_by(del_word_pair_proximity.iter(), add_word_pair_proximity.iter(), |d, a| {
            d.cmp(a)
        })
    {
        buffer.clear();
        let mut value_writer = KvWriterDelAdd::new(&mut buffer);
        let ((w1, w2), prox) = match eob {
            Left(key_value) => {
                value_writer.insert(DelAdd::Deletion, document_id.to_ne_bytes()).unwrap();
                key_value
            }
            Right(key_value) => {
                value_writer.insert(DelAdd::Addition, document_id.to_ne_bytes()).unwrap();
                key_value
            }
            Both(key_value, _) => {
                value_writer.insert(DelAdd::Deletion, document_id.to_ne_bytes()).unwrap();
                value_writer.insert(DelAdd::Addition, document_id.to_ne_bytes()).unwrap();
                key_value
            }
        };

        key_buffer.clear();
        key_buffer.push(*prox);
        key_buffer.extend_from_slice(w1.as_bytes());
        key_buffer.push(0);
        key_buffer.extend_from_slice(w2.as_bytes());

        word_pair_proximity_docids_sorters[*prox as usize - 1]
            .insert(&key_buffer, value_writer.into_inner().unwrap())?;
    }

    Ok(())
}

fn word_positions_into_word_pair_proximity(
    word_positions: &mut VecDeque<(String, u16)>,
    word_pair_proximity: &mut BTreeMap<(String, String), u8>,
) -> Result<()> {
    let (head_word, head_position) = word_positions.pop_front().unwrap();
    for (word, position) in word_positions.iter() {
        let prox = index_proximity(head_position as u32, *position as u32) as u8;
        if prox > 0 && prox < MAX_DISTANCE as u8 {
            word_pair_proximity
                .entry((head_word.clone(), word.clone()))
                .and_modify(|p| {
                    *p = cmp::min(*p, prox);
                })
                .or_insert(prox);
        }
    }
    Ok(())
}

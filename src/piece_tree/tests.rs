use super::btree::ChildRef;
use super::history::RootEntry;
use super::session::{serialize_add_page, serialize_history_page};
use super::{
    editlog_path, parse_page, serialize_page, source_metadata, DiskPageStore, FragmentationStats,
    Piece, PiecePage, PieceSource, PieceTree, SessionMeta, SourceMetadata, EDITLOG_CACHE_PAGES,
    EDITLOG_HEADER_BYTES, EDITLOG_V3_HEADER_BYTES, RESIDENT_PAGES_FAILED_PERSISTENCE_CAP,
};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};

fn collect(tree: &PieceTree) -> Vec<(PieceSource, usize, usize, usize)> {
    tree.to_vec()
        .into_iter()
        .map(|piece| (piece.src, piece.start, piece.len, piece.line_breaks))
        .collect()
}

#[test]
fn insert_at_head_is_logical_and_coalesces_adjacent_add_pieces() {
    let mut tree = PieceTree::from_pieces(vec![Piece {
        src: PieceSource::Original,
        start: 0,
        len: 10,
        line_breaks: 1,
    }]);

    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };

    tree.insert(
        0,
        Piece {
            src: PieceSource::Add,
            start: 0,
            len: 3,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 3,
            len: 2,
            line_breaks: 0,
        },
        &mut split,
    )
    .unwrap();

    assert_eq!(
        collect(&tree),
        vec![
            (PieceSource::Add, 0, 5, 1),
            (PieceSource::Original, 0, 10, 1),
        ]
    );
    assert_eq!(tree.total_len(), 15);
    assert_eq!(tree.total_line_breaks(), 2);
}

#[test]
fn delete_range_splices_across_piece_boundaries() {
    let mut tree = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 4,
            line_breaks: 1,
        },
        Piece {
            src: PieceSource::Add,
            start: 0,
            len: 3,
            line_breaks: 1,
        },
        Piece {
            src: PieceSource::Original,
            start: 4,
            len: 5,
            line_breaks: 0,
        },
    ]);

    let mut trim = |piece: Piece, local_start: usize, local_end: usize| {
        let left_len = local_start.min(piece.len);
        let right_len = piece.len.saturating_sub(local_end.min(piece.len));
        let left = (left_len > 0).then_some(Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: usize::from(piece.line_breaks > 0 && left_len == piece.len),
        });
        let right = (right_len > 0).then_some(Piece {
            src: piece.src,
            start: piece.start + (piece.len - right_len),
            len: right_len,
            line_breaks: 0,
        });
        (left, right)
    };

    tree.delete_range(2, 6, &mut trim).unwrap();

    assert_eq!(
        collect(&tree),
        vec![
            (PieceSource::Original, 0, 2, 0),
            (PieceSource::Original, 5, 4, 0),
        ]
    );
    assert_eq!(tree.total_len(), 6);
}

#[test]
fn coalesce_adjacent_preserves_original_break_boundaries() {
    let mut pieces = vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 64,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Original,
            start: 64,
            len: 8,
            line_breaks: 1,
        },
        Piece {
            src: PieceSource::Original,
            start: 72,
            len: 64,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Add,
            start: 0,
            len: 2,
            line_breaks: 1,
        },
        Piece {
            src: PieceSource::Add,
            start: 2,
            len: 3,
            line_breaks: 0,
        },
    ];

    super::coalesce_adjacent(&mut pieces);

    assert_eq!(
        pieces,
        vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 64,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Original,
                start: 64,
                len: 8,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Original,
                start: 72,
                len: 64,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Add,
                start: 0,
                len: 5,
                line_breaks: 1,
            },
        ]
    );
}

#[test]
fn visit_range_only_emits_overlapping_piece_segments() {
    let tree = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 4,
            line_breaks: 1,
        },
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 4,
            line_breaks: 0,
        },
    ]);

    let mut seen = Vec::new();
    tree.visit_range(2, 6, |piece, local_start, local_end| {
        seen.push((
            piece.src,
            piece.start + local_start,
            local_end - local_start,
        ));
    });

    assert_eq!(
        seen,
        vec![(PieceSource::Original, 2, 2), (PieceSource::Add, 10, 2)]
    );
}

#[test]
fn visit_range_rev_emits_overlapping_piece_segments_in_reverse_order() {
    let tree = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 4,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 3,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Original,
            start: 4,
            len: 2,
            line_breaks: 0,
        },
    ]);

    let mut seen = Vec::new();
    tree.visit_range_rev_while(2, 8, |piece, local_start, local_end| {
        seen.push((
            piece.src,
            piece.start + local_start,
            local_end - local_start,
        ));
        true
    });

    assert_eq!(
        seen,
        vec![
            (PieceSource::Original, 4, 1),
            (PieceSource::Add, 10, 3),
            (PieceSource::Original, 2, 2),
        ]
    );
}

#[test]
fn visit_range_while_stops_after_callback_returns_false() {
    let tree = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 4,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 3,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Original,
            start: 20,
            len: 2,
            line_breaks: 0,
        },
    ]);

    let mut seen = Vec::new();
    let completed = tree.visit_range_while(0, 9, |piece, local_start, local_end| {
        seen.push((
            piece.src,
            piece.start + local_start,
            local_end.saturating_sub(local_start),
        ));
        seen.len() < 2
    });

    assert!(!completed);
    assert_eq!(
        seen,
        vec![(PieceSource::Original, 0, 4), (PieceSource::Add, 10, 3)]
    );
}

#[test]
fn visit_range_rev_while_stops_after_callback_returns_false() {
    let tree = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 4,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 3,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Original,
            start: 20,
            len: 2,
            line_breaks: 0,
        },
    ]);

    let mut seen = Vec::new();
    let completed = tree.visit_range_rev_while(0, 9, |piece, local_start, local_end| {
        seen.push((
            piece.src,
            piece.start + local_start,
            local_end.saturating_sub(local_start),
        ));
        seen.len() < 2
    });

    assert!(!completed);
    assert_eq!(
        seen,
        vec![(PieceSource::Original, 20, 2), (PieceSource::Add, 10, 3)]
    );
}

#[test]
fn find_line_start_uses_weighted_breaks() {
    let tree = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 4,
            line_breaks: 1,
        },
        Piece {
            src: PieceSource::Original,
            start: 4,
            len: 5,
            line_breaks: 1,
        },
    ]);

    let found = tree.find_line_start(2, |piece, local_break| {
        if piece.start == 4 && local_break == 0 {
            Some(3)
        } else {
            None
        }
    });

    assert_eq!(found, Some(7));
}

#[test]
fn disk_backed_tree_round_trips_through_editlog() {
    let dir = std::env::temp_dir().join(format!("qem-piece-tree-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abc").unwrap();
    let tree = PieceTree::from_pieces_disk(
        &source,
        vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 1,
            },
            Piece {
                src: PieceSource::Add,
                start: 2,
                len: 3,
                line_breaks: 0,
            },
        ],
    )
    .unwrap();
    assert_eq!(tree.total_len(), 7);
    drop(tree);

    let reopened = PieceTree::open_disk(&source).unwrap();
    let sidecar = editlog_path(&source);
    assert_eq!(
        collect(&reopened),
        vec![
            (PieceSource::Original, 0, 4, 1),
            (PieceSource::Add, 2, 3, 0),
        ]
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_backed_tree_persists_latest_root_after_edit() {
    let dir = std::env::temp_dir().join(format!("qem-piece-tree-edit-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abc").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 6,
            line_breaks: 1,
        }],
    )
    .unwrap();

    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };

    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 2,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let reopened = PieceTree::open_disk(&source).unwrap();
    let sidecar = editlog_path(&source);
    assert_eq!(
        collect(&reopened),
        vec![
            (PieceSource::Original, 0, 3, 0),
            (PieceSource::Add, 10, 2, 1),
            (PieceSource::Original, 3, 3, 1),
        ]
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_open_truncates_orphan_pages_from_an_interrupted_flush() {
    // A crash between the page append and the header rewrite leaves
    // orphan pages past the committed size. Page writes append at
    // `SeekFrom::End` and assert the offset against the page id, so the
    // orphans would poison every later flush into `persistence_failed`
    // and silently disable persistence for the session. Open must
    // truncate back to the committed size (the header is the commit
    // record; the orphans are unreachable by design).
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-orphan-pages-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 2,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let sidecar = editlog_path(&source);
    {
        let mut file = fs::OpenOptions::new().append(true).open(&sidecar).unwrap();
        file.write_all(&[0u8; 2 * 4096]).unwrap();
    }

    let mut reopened = PieceTree::open_disk(&source).unwrap();
    reopened
        .insert(
            1,
            Piece {
                src: PieceSource::Add,
                start: 12,
                len: 1,
                line_breaks: 0,
            },
            &mut split,
        )
        .unwrap();
    reopened
        .flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(reopened);

    let again = PieceTree::open_disk(&source).unwrap();
    assert_eq!(
        again.total_len(),
        10,
        "the post-orphan edit must persist; a poisoned editlog would have \
             recovered the pre-edit session instead"
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_open_surfaces_corrupt_history_counters_as_typed_errors() {
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-corrupt-history-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 2,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let sidecar = editlog_path(&source);
    {
        let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
        file.seek(SeekFrom::Start(56)).unwrap();
        // history_page_count and history_len are raw header fields with
        // no checksum. A corrupt sidecar must surface as a typed error
        // instead of driving huge allocations or out-of-bounds reads.
        file.write_all(&(u64::MAX / 2).to_le_bytes()).unwrap();
        file.write_all(&(u64::MAX / 2).to_le_bytes()).unwrap();
        file.flush().unwrap();
    }

    let err = PieceTree::open_disk(&source).unwrap_err();
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
    ));

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_open_rejects_page_count_below_referenced_pages() {
    // `page_count` is a raw unchecksummed header field that drives
    // the committed-size truncation. A corrupt-small value must not
    // silently truncate pages the history still references (lazy
    // page misses then read short while total_len stays full); it
    // must surface as a typed error so the session sidecar is
    // dropped and rebuilt instead.
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-page-count-small-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 2,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let sidecar = editlog_path(&source);
    {
        let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
        file.seek(SeekFrom::Start(16)).unwrap();
        // Real sessions span several pages (piece page, history page,
        // add page); 1 is below every referenced page id.
        file.write_all(&1u64.to_le_bytes()).unwrap();
        file.flush().unwrap();
    }

    let err = PieceTree::open_disk(&source).unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidData,
        "a page_count below the referenced pages must reject the session"
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_open_rejects_page_count_beyond_the_sidecar_size() {
    // A corrupt-huge `page_count` must not become `next_page_id` (the
    // next flush computes page offsets with a non-saturating
    // multiply) nor extend the sidecar at open. It is an invalid
    // session, not a truncation hint.
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-page-count-huge-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 2,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let sidecar = editlog_path(&source);
    {
        let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
        file.seek(SeekFrom::Start(16)).unwrap();
        file.write_all(&u64::MAX.to_le_bytes()).unwrap();
        file.flush().unwrap();
    }

    let err = PieceTree::open_disk(&source).unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidData,
        "a page_count beyond the sidecar size must reject the session"
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

/// Builds a small disk-backed session (original `abcdef\n` plus a
/// 2-byte insert, committed root total 9 bytes) flushed with the given
/// session metadata, so a test can shape the header totals that the
/// corruption below mutates.
fn flushed_disk_session_with_meta(
    dir_name: &str,
    meta: SessionMeta,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("qem-piece-tree-{dir_name}-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 2,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", meta).unwrap();
    drop(tree);
    let sidecar = editlog_path(&source);
    (source, sidecar)
}

fn corrupt_header_u64(sidecar: &std::path::Path, offset: u64, value: u64) {
    let mut file = fs::OpenOptions::new().write(true).open(sidecar).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&value.to_le_bytes()).unwrap();
    file.flush().unwrap();
}

#[test]
fn disk_open_rejects_known_byte_len_above_the_committed_root() {
    // `known_byte_len` is a raw unchecksummed header field that
    // recovery clamps against the committed root size
    // (`meta.known_byte_len.min(total_len)`), so a corrupt value above
    // the root's total byte count used to open as a session whose
    // known-index boundary silently disagreed with the content it
    // advertises. The known boundary is a prefix of the document, so
    // it can never legally exceed the committed root: reject the
    // session so the sidecar is dropped and rebuilt.
    let (source, sidecar) = flushed_disk_session_with_meta(
        "known-byte-len-above-root",
        SessionMeta {
            known_byte_len: 4,
            full_index: false,
            ..SessionMeta::default()
        },
    );

    // The committed root totals 9 bytes (7 original + 2 add); a
    // partial-index boundary of 4 is legal, 73 is not.
    corrupt_header_u64(&sidecar, 104, 73);

    let err = PieceTree::open_disk(&source).unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidData,
        "a known_byte_len above the committed root must reject the session"
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(source.parent().unwrap());
}

#[test]
fn disk_open_rejects_full_index_known_byte_len_below_the_committed_root() {
    // A full-index session carries the exact line index for the whole
    // document, so its flushed `known_byte_len` always equals the
    // committed root's total byte count (the edit paths maintain
    // `known_byte_len == total_len` whenever `full_index` is set). A
    // corrupt header that breaks that equality would reopen with a
    // full index claiming exact line counts over a shorter known
    // boundary; reject instead of silently clamping.
    let (source, sidecar) = flushed_disk_session_with_meta(
        "known-byte-len-full-index",
        SessionMeta {
            known_byte_len: 9,
            full_index: true,
            ..SessionMeta::default()
        },
    );

    // One byte below the committed root total of 9: still a legal
    // partial-index boundary, so only the full-index equality rule
    // can catch this divergence.
    corrupt_header_u64(&sidecar, 104, 8);

    let err = PieceTree::open_disk(&source).unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidData,
        "a full-index known_byte_len below the committed root must reject the session"
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(source.parent().unwrap());
}

#[test]
fn disk_edit_fails_typed_when_a_piece_page_is_unreadable() {
    // A lazily-read piece page whose bytes no longer pass the page
    // CRC must not let a structural edit silently splice the
    // subtree out: an unreadable page used to answer an empty child
    // list, the parent recomputed its totals over the survivors,
    // and the next edit shrank the document while `total_len` still
    // advertised the full size. The edit must fail as a typed error
    // so the caller keeps the tree intact.
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-unreadable-piece-page-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let sidecar = editlog_path(&source);
    {
        // Piece page 1 holds the original pieces; scramble its
        // payload so the page CRC fails on the next lazy read.
        let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
        file.seek(SeekFrom::Start((EDITLOG_HEADER_BYTES + 8) as u64))
            .unwrap();
        file.write_all(&[0xFFu8; 16]).unwrap();
        file.flush().unwrap();
    }

    // Since open-time verification exists, a session whose live root
    // reaches the unreadable page must already fail `open` as a
    // typed error (the recovery layer then drops the sidecar and
    // rebuilds from the source). The edit-time refusal below stays
    // as defense in depth for a page that turns unreadable only
    // after a successful open, so both outcomes are accepted here.
    match PieceTree::open_disk(&source) {
            Err(err) => assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidData,
                "open-time verification must reject the unreadable piece page as a typed error, got {:?}",
                err.kind()
            ),
            Ok(mut reopened) => {
                let insert_result = reopened.insert(
                    3,
                    Piece {
                        src: PieceSource::Add,
                        start: 10,
                        len: 2,
                        line_breaks: 1,
                    },
                    &mut split,
                );
                assert!(
                    insert_result.is_err(),
                    "editing through an unreadable piece page must fail, not lose the subtree"
                );
                assert_eq!(
                    reopened.total_len(),
                    7,
                    "the refused edit must leave the tree content intact"
                );
            }
        }

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_open_rejects_a_session_with_an_unreadable_piece_page() {
    // A lazily-read piece page that no longer passes its CRC answers
    // `None` to every reader, so a session whose live root reaches
    // such a page used to open fine and advertised `total_len` over
    // content the reads silently skipped. Open must verify every page
    // reachable from the committed root up front and reject the
    // session as a typed error, letting the recovery layer drop the
    // sidecar and rebuild from the source file.
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-open-unreadable-piece-page-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![
            Piece {
                src: PieceSource::Original,
                start: 0,
                len: 4,
                line_breaks: 0,
            },
            Piece {
                src: PieceSource::Original,
                start: 4,
                len: 3,
                line_breaks: 1,
            },
        ],
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let sidecar = editlog_path(&source);
    {
        // Piece page 1 holds the original pieces; scramble its
        // payload so the page CRC fails on the next lazy read.
        let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
        file.seek(SeekFrom::Start((EDITLOG_HEADER_BYTES + 8) as u64))
            .unwrap();
        file.write_all(&[0xFFu8; 16]).unwrap();
        file.flush().unwrap();
    }

    let err = PieceTree::open_disk(&source).unwrap_err();
    assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "opening a session whose live root reaches an unreadable piece page must fail as a typed error"
        );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_open_survives_a_corrupt_history_page_id_as_typed_error() {
    // `history_first_page_id` is another raw unchecksummed header
    // field. A corrupt-huge value must not overflow the page-offset
    // arithmetic (debug builds panic on the multiply), it must
    // surface as a typed IO error like every other corrupt counter.
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-corrupt-history-page-id-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.insert(
        3,
        Piece {
            src: PieceSource::Add,
            start: 10,
            len: 2,
            line_breaks: 1,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    drop(tree);

    let sidecar = editlog_path(&source);
    {
        let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
        file.seek(SeekFrom::Start(48)).unwrap();
        file.write_all(&u64::MAX.to_le_bytes()).unwrap();
        file.flush().unwrap();
    }

    let err = PieceTree::open_disk(&source).unwrap_err();
    assert!(
        matches!(
                err.kind(),
                std::io::ErrorKind::InvalidData
                    | std::io::ErrorKind::UnexpectedEof
                    // Linux lseek rejects the out-of-range offset with
                    // EINVAL, which std maps to InvalidInput.
                    | std::io::ErrorKind::InvalidInput
            )
            // Windows maps the out-of-range page-id seek to raw OS
            // errors instead of UnexpectedEof: 131 =
            // ERROR_NEGATIVE_SEEK, 38 = ERROR_HANDLE_EOF.
            || matches!(err.raw_os_error(), Some(38) | Some(131)),
        "actual kind: {:?}, raw: {:?}",
        err.kind(),
        err.raw_os_error()
    );

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn parse_page_rejects_empty_internal_pages_as_invalid() {
    // The writer never emits an empty internal page; a CRC-valid
    // forged one would panic the insert path and silently empty the
    // document on delete, so parse must reject it up front.
    let page = PiecePage::Internal {
        children: Vec::new(),
    };
    let buf = serialize_page(&page).unwrap();
    let err = parse_page(&buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn parse_page_rejects_empty_leaf_pages_as_invalid() {
    // Symmetric with the internal-page reject: the writer never
    // emits an empty leaf either.
    let page = PiecePage::Leaf { pieces: Vec::new() };
    let buf = serialize_page(&page).unwrap();
    let err = parse_page(&buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn disk_resident_window_refuses_edits_before_losing_pages() {
    // While persistence is down every new piece page lives only in
    // the bounded resident window (4096). An edit whose page the
    // window refused must fail typed instead of landing: otherwise
    // the recorded root would reference unreadable pages and reads
    // would silently shorten behind `total_len` once the page cache
    // (1024) evicted them too.
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-resident-window-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    tree.poison_persistence_for_test();

    // Single-byte inserts at the tail; every copy-on-write rebuild
    // allocates a fresh page, so the refusal must arrive around the
    // window size instead of never.
    let mut accepted = 0usize;
    let mut refused = false;
    for i in 0..(RESIDENT_PAGES_FAILED_PERSISTENCE_CAP * 2) {
        let insert_result = tree.insert(
            7 + accepted,
            Piece {
                src: PieceSource::Add,
                start: i,
                len: 1,
                line_breaks: 0,
            },
            &mut split,
        );
        match insert_result {
            Ok(()) => accepted += 1,
            Err(_) => {
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "the resident window must eventually refuse edits");
    assert!(
        accepted >= RESIDENT_PAGES_FAILED_PERSISTENCE_CAP - 64,
        "the window must still give edits room before refusing (accepted {accepted})"
    );
    assert_eq!(
        tree.total_len(),
        7 + accepted,
        "the refused edit must not change the tree size"
    );

    // Reads must stay complete: walking the live root collects every
    // piece, so a silent unreadable-page skip would shrink the sum
    // below total_len.
    let pieces = tree.to_vec();
    let collected_bytes: usize = pieces.iter().map(|piece| piece.len).sum();
    assert_eq!(
        collected_bytes,
        7 + accepted,
        "every referenced page must stay readable past the refusal"
    );

    let sidecar = editlog_path(&source);
    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_resident_window_verifies_the_rebuilt_root() {
    // The root rebuild itself allocates pages on an overflowing edit
    // (a full leaf splitting into two plus a new internal page). The
    // residency verification must see those pages too: an edit whose
    // rebuilt root references a page the window refused must fail,
    // otherwise the root points at a page that only the page cache
    // keeps readable - and reads silently shrink once it evicts.
    //
    // Deterministic setup: the window is filled to exactly one free
    // slot, the tree already spans several leaves (so the rebuild
    // allocates an internal page), and the next edit therefore
    // occupies the last slot with its insert_at page while the
    // rebuilt-root page lands beyond the window.
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-resident-root-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = |piece: Piece, left_len: usize| {
        let left = Piece {
            src: piece.src,
            start: piece.start,
            len: left_len,
            line_breaks: 0,
        };
        let right = Piece {
            src: piece.src,
            start: piece.start + left_len,
            len: piece.len - left_len,
            line_breaks: piece.line_breaks,
        };
        (Some(left), Some(right))
    };
    tree.flush_session(b"ignored-by-test", SessionMeta::default())
        .unwrap();
    tree.poison_persistence_for_test();

    // Non-coalescing tail inserts (the add-buffer start skips a byte
    // per insert, so `coalesce_adjacent` never merges them) grow the
    // piece count past the first leaf split, making every further
    // rebuild allocate an internal page.
    let mut accepted = 0usize;
    for _ in 0..200 {
        tree.insert(
            7 + accepted,
            Piece {
                src: PieceSource::Add,
                start: accepted.saturating_mul(2),
                len: 1,
                line_breaks: 0,
            },
            &mut split,
        )
        .unwrap();
        accepted += 1;
    }

    // Fill the window so the next edit gets exactly one free slot:
    // its insert_at page takes it, and the rebuilt-root page (the
    // internal above the split leaves) must fall outside the window
    // and refuse the edit.
    tree.fill_resident_window_for_test(RESIDENT_PAGES_FAILED_PERSISTENCE_CAP - 1);
    let insert_result = tree.insert(
        7 + accepted,
        Piece {
            src: PieceSource::Add,
            start: accepted.saturating_mul(2),
            len: 1,
            line_breaks: 0,
        },
        &mut split,
    );
    assert!(
        insert_result.is_err(),
        "the rebuilt-root page beyond the window must refuse the edit"
    );

    // Refused edits still allocate (the tree walk runs before the
    // verification), so churning the page cache evicts anything a
    // stale implementation kept readable.
    for _ in 0..(EDITLOG_CACHE_PAGES * 2) {
        let _ = tree.insert(
            7 + accepted,
            Piece {
                src: PieceSource::Add,
                start: RESIDENT_PAGES_FAILED_PERSISTENCE_CAP.saturating_mul(4),
                len: 1,
                line_breaks: 0,
            },
            &mut split,
        );
    }

    assert_eq!(
        tree.total_len(),
        7 + accepted,
        "the refused edit must not change the tree size"
    );
    let pieces = tree.to_vec();
    let collected_bytes: usize = pieces.iter().map(|piece| piece.len).sum();
    assert_eq!(
        collected_bytes,
        7 + accepted,
        "every page referenced by the live root must stay readable - \
             a post-verify allocation escaping the window check shrinks reads"
    );

    let sidecar = editlog_path(&source);
    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disk_open_rejects_stale_content_fingerprint_even_with_matching_file_metadata() {
    let dir = std::env::temp_dir().join(format!(
        "qem-piece-tree-stale-fingerprint-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    drop(tree);

    let source_meta = source_metadata(&source).unwrap();
    let stale = SourceMetadata {
        len: source_meta.len,
        mtime_ns: source_meta.mtime_ns,
        fingerprint: source_meta.fingerprint ^ 1,
    };
    let reopened = DiskPageStore::open(&source, stale).unwrap();
    assert!(reopened.is_none());

    let sidecar = editlog_path(&source);
    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fragmentation_stats_report_piece_size_distribution() {
    let tree = PieceTree::from_pieces(vec![
        Piece {
            src: PieceSource::Original,
            start: 0,
            len: 4096,
            line_breaks: 1,
        },
        Piece {
            src: PieceSource::Add,
            start: 0,
            len: 16,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Add,
            start: 64,
            len: 12,
            line_breaks: 0,
        },
        Piece {
            src: PieceSource::Original,
            start: 4096,
            len: 8192,
            line_breaks: 2,
        },
    ]);

    let stats = tree.fragmentation_stats_with_threshold(64);

    assert_eq!(
        stats,
        FragmentationStats {
            piece_count: 4,
            total_bytes: 12_316,
            smallest_piece_bytes: 12,
            largest_piece_bytes: 8192,
            small_piece_threshold_bytes: 64,
            small_piece_count: 2,
            small_piece_bytes: 28,
        }
    );
    assert!((stats.average_piece_bytes() - 3079.0).abs() < f64::EPSILON);
    assert!((stats.fragmentation_ratio() - 0.5).abs() < f64::EPSILON);
}

#[test]
fn fragmentation_stats_for_empty_tree_are_zeroed() {
    let stats = PieceTree::new().fragmentation_stats();

    assert_eq!(
        stats,
        FragmentationStats {
            small_piece_threshold_bytes: super::DEFAULT_FRAGMENTATION_SMALL_PIECE_BYTES,
            ..FragmentationStats::default()
        }
    );
    assert_eq!(stats.average_piece_bytes(), 0.0);
    assert_eq!(stats.fragmentation_ratio(), 0.0);
}

#[test]
fn bounded_page_count_caps_counters_by_physical_file_size() {
    use super::{bounded_page_count, EDITLOG_HEADER_BYTES, EDITLOG_PAGE_SIZE};

    let header_only = EDITLOG_HEADER_BYTES as u64;
    // A sidecar that only holds the header cannot hold any pages.
    assert_eq!(
        bounded_page_count(Some(header_only), 5, EDITLOG_HEADER_BYTES as u64),
        0
    );
    // Without a readable file length the raw counter must never reach an
    // allocation: the chain is treated as empty instead.
    assert_eq!(
        bounded_page_count(None, usize::MAX, EDITLOG_HEADER_BYTES as u64),
        0
    );

    let three_pages = header_only + (EDITLOG_PAGE_SIZE as u64) * 3;
    assert_eq!(
        bounded_page_count(Some(three_pages), 100, EDITLOG_HEADER_BYTES as u64),
        3
    );
    assert_eq!(
        bounded_page_count(Some(three_pages), 2, EDITLOG_HEADER_BYTES as u64),
        2
    );
    // Huge files clamp to the caller-provided counter, not the other way.
    assert_eq!(
        bounded_page_count(Some(u64::MAX), 7, EDITLOG_HEADER_BYTES as u64),
        7
    );
}

fn flush_session_with_encoding(
    tree: &mut PieceTree,
    encoding: Option<crate::document::DocumentEncoding>,
) {
    // The fixture roots total 6 bytes, and open cross-validates a
    // full-index `known_byte_len` against that total, so the flushed
    // metadata must stay consistent (a full index always covers the
    // whole document).
    tree.flush_session(
        b"add",
        SessionMeta {
            known_byte_len: 6,
            full_index: true,
            encoding,
            encoding_origin: Some(crate::document::DocumentEncodingOrigin::Utf8FastPath),
            decoding_had_errors: false,
        },
    )
    .unwrap();
}

fn temp_editlog_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("qem-editlog-{}-{}", label, std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn read_sidecar_header_bytes(path: &std::path::Path) -> Vec<u8> {
    let mut file = fs::File::open(path).unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    buf
}

#[test]
fn editlog_session_round_trips_the_encoding_label() {
    let dir = temp_editlog_dir("encoding-label");
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 6,
            line_breaks: 1,
        }],
    )
    .unwrap();

    let windows_1251 =
        crate::document::DocumentEncoding::from_label("windows-1251").expect("known label");
    flush_session_with_encoding(&mut tree, Some(windows_1251));
    drop(tree);

    // The label bytes land in the v3 header tail, NUL-padded.
    let header = read_sidecar_header_bytes(&editlog_path(&source));
    let label =
        &header[super::EDITLOG_ENCODING_LABEL_OFFSET..super::EDITLOG_ENCODING_LABEL_OFFSET + 13];
    assert_eq!(label, b"windows-1251\x00");

    let storage = crate::storage::FileStorage::open(&source).unwrap();
    let (_, _, meta) = PieceTree::try_open_disk_session(&source, &storage)
        .unwrap()
        .expect("recoverable session");
    assert_eq!(meta.encoding, Some(windows_1251));

    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editlog_open_rejects_a_corrupt_encoding_label_as_invalid_data() {
    let dir = temp_editlog_dir("corrupt-label");
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 6,
            line_breaks: 1,
        }],
    )
    .unwrap();
    flush_session_with_encoding(&mut tree, Some(crate::document::DocumentEncoding::utf8()));
    drop(tree);

    // Overwrite the v3 label with bytes that are neither valid UTF-8 nor
    // a known label: the header is the commit record and must reject the
    // session so recovery drops and rebuilds the sidecar.
    let sidecar = editlog_path(&source);
    let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
    file.seek(SeekFrom::Start(super::EDITLOG_ENCODING_LABEL_OFFSET as u64))
        .unwrap();
    file.write_all(&[0xFF; super::EDITLOG_ENCODING_LABEL_BYTES])
        .unwrap();
    drop(file);

    let storage = crate::storage::FileStorage::open(&source).unwrap();
    let err = PieceTree::try_open_disk_session(&source, &storage).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editlog_open_rejects_a_legacy_v2_sidecar_as_unsupported() {
    let dir = temp_editlog_dir("legacy-v2");
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 6,
            line_breaks: 1,
        }],
    )
    .unwrap();
    flush_session_with_encoding(&mut tree, Some(crate::document::DocumentEncoding::utf8()));
    drop(tree);

    // Patch the version field back to v2: the historical sidecar carried
    // no encoding label and is rejected by the version check so recovery
    // drops and rebuilds it from the source file.
    let sidecar = editlog_path(&source);
    let mut file = fs::OpenOptions::new().write(true).open(&sidecar).unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    file.write_all(&2u32.to_le_bytes()).unwrap();
    drop(file);

    let storage = crate::storage::FileStorage::open(&source).unwrap();
    let err = PieceTree::try_open_disk_session(&source, &storage).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

// --- v4 append-only sidecar format ---

fn btree_split(piece: Piece, left_len: usize) -> (Option<Piece>, Option<Piece>) {
    let left = Piece {
        src: piece.src,
        start: piece.start,
        len: left_len,
        line_breaks: 0,
    };
    let right = Piece {
        src: piece.src,
        start: piece.start + left_len,
        len: piece.len - left_len,
        line_breaks: piece.line_breaks,
    };
    (Some(left), Some(right))
}

fn flush_meta(tree: &PieceTree) -> SessionMeta {
    SessionMeta {
        known_byte_len: tree.total_len(),
        full_index: true,
        encoding: None,
        encoding_origin: Some(crate::document::DocumentEncodingOrigin::Utf8FastPath),
        decoding_had_errors: false,
    }
}

/// Regression for the sidecar growth explosion: a degenerate page split
/// (`[64, 1]` on every overflow) turned repeated inserts at one end into a
/// "ladder" tree whose page path - rewritten by every edit - grew linearly
/// with the edit count, so the sidecar paid quadratic bytes. The balanced
/// split keeps the per-edit page cost flat, so the second half of a stress
/// run must not outgrow the first half by more than its length ratio.
#[test]
fn inserts_at_one_end_keep_the_sidecar_growth_linear() {
    let dir = temp_editlog_dir("linear-growth");
    let source = dir.join("sample.txt");
    let original_len = 80 * 1024;
    let mut bytes = vec![0u8; original_len];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = if (i + 1) % 5 == 0 { b'\n' } else { b'x' };
    }
    fs::write(&source, &bytes).unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: original_len,
            line_breaks: original_len / 5,
        }],
    )
    .unwrap();
    let mut split = btree_split;
    let mut add: Vec<u8> = Vec::new();
    let sidecar = editlog_path(&source);

    for i in 0..500usize {
        tree.insert(
            0,
            Piece {
                src: PieceSource::Add,
                start: add.len(),
                len: 3,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        add.extend_from_slice(b"ab\n");
        if (i + 1) % 50 == 0 {
            tree.flush_session(&add, flush_meta(&tree)).unwrap();
        }
    }
    let at_500 = fs::metadata(&sidecar).unwrap().len();
    for i in 500..2000usize {
        tree.insert(
            0,
            Piece {
                src: PieceSource::Add,
                start: add.len(),
                len: 3,
                line_breaks: 1,
            },
            &mut split,
        )
        .unwrap();
        add.extend_from_slice(b"ab\n");
        if (i + 1) % 50 == 0 {
            tree.flush_session(&add, flush_meta(&tree)).unwrap();
        }
    }
    let at_2000 = fs::metadata(&sidecar).unwrap().len();

    let first_half = at_500;
    let second_half = at_2000 - at_500;
    // Linear growth: the second half covers 3x the edits of the first half.
    // The degenerate ladder paid ~15x (quadratic); allow a generous 5x.
    assert!(
        second_half <= first_half * 5 + 1_048_576,
        "second-half sidecar growth {} bytes must stay linear against first-half {} bytes",
        second_half,
        first_half
    );
    assert!(
        at_2000 < 60 * 1024 * 1024,
        "2000 edits must not inflate the sidecar to {} bytes",
        at_2000
    );

    // The rebuilt tree must still answer correctly after all that churn.
    drop(tree);
    let reopened = PieceTree::open_disk(&source).unwrap();
    assert_eq!(reopened.total_len(), original_len + 2000 * 3);

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

/// v4 keeps the redo tail across reopen (the committed epoch marks the live
/// entries), while a new edit after undo truncates the tail for good.
#[test]
fn redo_tail_survives_reopen_and_branch_truncation_ends_it() {
    let dir = temp_editlog_dir("redo-tail");
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let mut tree = PieceTree::from_pieces_disk(
        &source,
        vec![Piece {
            src: PieceSource::Original,
            start: 0,
            len: 7,
            line_breaks: 1,
        }],
    )
    .unwrap();
    let mut split = btree_split;
    let mut add: Vec<u8> = Vec::new();
    for chunk in [&b"11"[..], &b"22"[..], &b"33"[..]] {
        tree.insert(
            0,
            Piece {
                src: PieceSource::Add,
                start: add.len(),
                len: 2,
                line_breaks: 0,
            },
            &mut split,
        )
        .unwrap();
        add.extend_from_slice(chunk);
    }
    tree.flush_session(&add, flush_meta(&tree)).unwrap();
    drop(tree);

    // Reopen, undo back to the base state, flush: the redo tail must survive.
    let mut reopened = PieceTree::open_disk(&source).unwrap();
    assert_eq!(reopened.total_len(), 13);
    for _ in 0..3 {
        assert!(reopened.undo((7, false)).is_some());
    }
    assert_eq!(reopened.total_len(), 7);
    reopened.flush_session(&add, flush_meta(&reopened)).unwrap();
    drop(reopened);

    let mut again = PieceTree::open_disk(&source).unwrap();
    let redo_expectations = [9usize, 11, 13];
    for expected in redo_expectations {
        assert!(
            again.redo((7, false)).is_some(),
            "redo tail must survive the reopen"
        );
        assert_eq!(again.total_len(), expected);
    }
    drop(again);

    // Branch after undo: the redo tail is truncated and never comes back.
    // The reopen stands on the committed position (the base state), so redo
    // back to the tip first.
    let mut branch = PieceTree::open_disk(&source).unwrap();
    assert_eq!(branch.total_len(), 7);
    for expected in [9usize, 11, 13] {
        assert!(branch.redo((7, false)).is_some());
        assert_eq!(branch.total_len(), expected);
    }
    for _ in 0..3 {
        assert!(branch.undo((7, false)).is_some());
    }
    branch
        .insert(
            0,
            Piece {
                src: PieceSource::Add,
                start: add.len(),
                len: 2,
                line_breaks: 0,
            },
            &mut split,
        )
        .unwrap();
    let mut branched_add = add.clone();
    branched_add.extend_from_slice(b"44");
    branch
        .flush_session(&branched_add, flush_meta(&branch))
        .unwrap();
    drop(branch);

    let mut after_branch = PieceTree::open_disk(&source).unwrap();
    assert_eq!(after_branch.total_len(), 9);
    assert!(
        after_branch.redo((9, false)).is_none(),
        "a branched history must not resurrect the abandoned redo tail"
    );

    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

/// Legacy v3 sidecars (contiguous history/add blocks behind a 192-byte
/// header) still recover, and the first flush converts them to the
/// append-only format without losing the unsaved edits.
#[test]
fn legacy_v3_sidecar_converts_on_the_first_flush() {
    let dir = temp_editlog_dir("v3-convert");
    let source = dir.join("sample.txt");
    fs::write(&source, b"abcdef\n").unwrap();
    let source_meta = source_metadata(&source).unwrap();

    // Hand-assemble a v3 sidecar: one original leaf, one add leaf, an
    // internal root over both, then the contiguous history and add blocks.
    // Page layout: 1 = original leaf, 2 = add leaf, 3 = root, 4 = history,
    // 5 = add bytes.
    let orig_piece = Piece {
        src: PieceSource::Original,
        start: 0,
        len: 7,
        line_breaks: 1,
    };
    let add_piece = Piece {
        src: PieceSource::Add,
        start: 0,
        len: 2,
        line_breaks: 0,
    };
    let orig_leaf_ref = ChildRef {
        page_id: 1,
        total_bytes: 7,
        total_line_breaks: 1,
    };
    let add_leaf_ref = ChildRef {
        page_id: 2,
        total_bytes: 2,
        total_line_breaks: 0,
    };
    let root_ref = ChildRef {
        page_id: 3,
        total_bytes: 9,
        total_line_breaks: 1,
    };
    let pages: Vec<Vec<u8>> = vec![
        serialize_page(&PiecePage::Leaf {
            pieces: vec![orig_piece],
        })
        .unwrap(),
        serialize_page(&PiecePage::Leaf {
            pieces: vec![add_piece],
        })
        .unwrap(),
        serialize_page(&PiecePage::Internal {
            children: vec![add_leaf_ref, orig_leaf_ref],
        })
        .unwrap(),
        serialize_history_page(&[
            RootEntry {
                root: Some(orig_leaf_ref),
            },
            RootEntry {
                root: Some(root_ref),
            },
        ])
        .unwrap(),
        serialize_add_page(b"ab").unwrap(),
    ];

    // Hand-assembled 192-byte v3 header: the production v3 writer is gone
    // (v4 is the only written format), so the fixture builds the raw layout.
    let mut header = [0u8; EDITLOG_V3_HEADER_BYTES];
    header[0..8].copy_from_slice(b"QEMEDT1\0");
    header[8..12].copy_from_slice(&3u32.to_le_bytes());
    header[12..16].copy_from_slice(&(super::EDITLOG_PAGE_SIZE as u32).to_le_bytes());
    header[16..24].copy_from_slice(&(pages.len() as u64).to_le_bytes());
    header[24..32].copy_from_slice(&source_meta.len.to_le_bytes());
    header[32..40].copy_from_slice(&source_meta.mtime_ns.to_le_bytes());
    header[40..48].copy_from_slice(&source_meta.fingerprint.to_le_bytes());
    header[48..56].copy_from_slice(&4u64.to_le_bytes()); // history first page
    header[56..64].copy_from_slice(&1u64.to_le_bytes()); // history page count
    header[64..72].copy_from_slice(&2u64.to_le_bytes()); // history len
    header[72..80].copy_from_slice(&1u64.to_le_bytes()); // history index
    header[80..88].copy_from_slice(&5u64.to_le_bytes()); // add first page
    header[88..96].copy_from_slice(&1u64.to_le_bytes()); // add page count
    header[96..104].copy_from_slice(&2u64.to_le_bytes()); // add len
    header[104..112].copy_from_slice(&9u64.to_le_bytes()); // known_byte_len
    header[112..120].copy_from_slice(&super::EDITLOG_FLAG_FULL_INDEX.to_le_bytes());
    let sidecar = editlog_path(&source);
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&sidecar)
            .unwrap();
        file.write_all(&header).unwrap();
        for page in &pages {
            file.write_all(page).unwrap();
        }
        file.sync_all().unwrap();
    }

    // v3 recovery: the unsaved edits (the "ab" insert) must come back.
    let mut tree = PieceTree::open_disk(&source).unwrap();
    assert_eq!(tree.total_len(), 9);

    // A new edit + flush converts the sidecar to v4.
    let mut split = btree_split;
    tree.insert(
        0,
        Piece {
            src: PieceSource::Add,
            start: 2,
            len: 2,
            line_breaks: 0,
        },
        &mut split,
    )
    .unwrap();
    tree.flush_session(b"abcc", flush_meta(&tree)).unwrap();
    drop(tree);

    // v4 recovery of the converted sidecar: full history including undo.
    let mut reopened = PieceTree::open_disk(&source).unwrap();
    assert_eq!(reopened.total_len(), 11);
    assert!(reopened.undo((11, false)).is_some());
    assert_eq!(reopened.total_len(), 9);
    assert!(reopened.undo((9, false)).is_some());
    assert_eq!(reopened.total_len(), 7);
    drop(reopened);

    // Reopen once more: the flushed state is the converted v4 snapshot.
    let final_tree = PieceTree::open_disk(&source).unwrap();
    assert_eq!(final_tree.total_len(), 11);

    let _ = fs::remove_file(&sidecar);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_dir_all(&dir);
}

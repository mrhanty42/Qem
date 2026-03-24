use super::{
    BackgroundActivity, BackgroundIssueKind, CursorPosition, DocumentSession, EditorTab, LoadPhase,
    SaveError,
};
use crate::{
    DocumentBacking, EditCapability, LiteralSearchQuery, TextPosition, TextSelection,
    ViewportRequest,
};
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn save_async_completes_and_clears_dirty_flag() {
    let dir = std::env::temp_dir().join(format!("qem-editor-save-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("large.txt");
    let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
    fs::write(&path, &original).unwrap();

    let mut tab = EditorTab::new(1);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.document_mut().try_insert_text_at(0, 0, "123").unwrap();

    assert!(tab.is_dirty());
    assert!(tab.save_async().unwrap());
    assert!(tab.is_saving());
    assert!(tab.is_busy());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = tab.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "async save timed out");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!tab.is_dirty());
    assert!(!tab.is_saving());
    assert!(fs::read(&path).unwrap().starts_with(b"123abc\ndef\n"));

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn session_and_tab_find_next_delegate_to_document_search() {
    let mut session = DocumentSession::new();
    let _ = session
        .document_mut()
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta\ngamma\nbeta")
        .unwrap();

    let session_found = session.find_next("beta", TextPosition::new(1, 1)).unwrap();
    assert_eq!(session_found.start(), TextPosition::new(3, 0));
    assert_eq!(session_found.end(), TextPosition::new(3, 4));

    let mut tab = EditorTab::new(1);
    let _ = tab
        .document_mut()
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta\ngamma\nbeta")
        .unwrap();

    let tab_found = tab.find_next("beta", TextPosition::new(1, 1)).unwrap();
    assert_eq!(tab_found.start(), TextPosition::new(3, 0));
    assert_eq!(tab_found.end(), TextPosition::new(3, 4));

    let session_prev = session.find_prev("beta", TextPosition::new(3, 4)).unwrap();
    assert_eq!(session_prev.start(), TextPosition::new(3, 0));
    assert_eq!(session_prev.end(), TextPosition::new(3, 4));

    let tab_prev = tab.find_prev("beta", TextPosition::new(3, 0)).unwrap();
    assert_eq!(tab_prev.start(), TextPosition::new(1, 0));
    assert_eq!(tab_prev.end(), TextPosition::new(1, 4));

    let query = LiteralSearchQuery::new("beta").unwrap();
    let session_query_found = session
        .find_next_query(&query, TextPosition::new(1, 1))
        .unwrap();
    assert_eq!(session_query_found.start(), TextPosition::new(3, 0));
    assert_eq!(session_query_found.end(), TextPosition::new(3, 4));

    let tab_query_prev = tab
        .find_prev_query(&query, TextPosition::new(3, 0))
        .unwrap();
    assert_eq!(tab_query_prev.start(), TextPosition::new(1, 0));
    assert_eq!(tab_query_prev.end(), TextPosition::new(1, 4));

    let range = session.text_range_between(TextPosition::new(1, 0), TextPosition::new(3, 0));
    let session_bounded = session.find_next_in_range("beta", range).unwrap();
    assert_eq!(session_bounded.start(), TextPosition::new(1, 0));
    assert_eq!(session_bounded.end(), TextPosition::new(1, 4));

    let tab_bounded = tab.find_prev_query_in_range(&query, range).unwrap();
    assert_eq!(tab_bounded.start(), TextPosition::new(1, 0));
    assert_eq!(tab_bounded.end(), TextPosition::new(1, 4));
}

#[test]
fn save_as_async_failure_preserves_dirty_state_and_clears_job() {
    let dir = std::env::temp_dir().join(format!("qem-editor-save-failure-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("large.txt");
    let missing_parent = dir.join("missing");
    let output = missing_parent.join("copy.txt");
    fs::write(&path, b"abc\ndef\n").unwrap();

    let mut tab = EditorTab::new(2);
    tab.open_file(path.clone()).unwrap();
    let generation = tab.generation();
    let _ = tab.document_mut().try_insert_text_at(0, 0, "123").unwrap();

    assert!(tab.save_as_async(output.clone()).unwrap());
    assert!(tab.is_saving());
    assert_eq!(
        tab.save_state().expect("save state should exist").path(),
        output.as_path()
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = tab.poll_save_job() {
            break result.unwrap_err();
        }
        assert!(Instant::now() < deadline, "async save failure timed out");
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(err, crate::DocumentError::Write { .. }));
    assert!(tab.is_dirty());
    assert!(!tab.is_saving());
    assert!(!tab.is_busy());
    assert_eq!(tab.generation(), generation);
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert!(tab.save_state().is_none());
    assert_eq!(fs::read(&path).unwrap(), b"abc\ndef\n");
    assert!(!output.exists());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_rejects_edits_while_async_save_is_in_progress() {
    let dir = std::env::temp_dir().join(format!("qem-editor-save-guard-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("guard.txt");
    let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
    fs::write(&path, &original).unwrap();

    let mut tab = EditorTab::new(16);
    tab.open_file(path.clone()).unwrap();
    let inserted = tab.try_insert(TextPosition::new(0, 0), "123");
    assert!(matches!(
        inserted,
        Ok(cursor) if cursor == TextPosition::new(0, 3)
    ));

    assert!(tab.save_async().unwrap());
    assert!(tab.is_saving());

    let err = tab.try_insert(TextPosition::new(0, 0), "Z").unwrap_err();
    assert!(matches!(
        err,
        crate::DocumentError::EditUnsupported {
            path: Some(ref blocked_path),
            reason,
        } if blocked_path == &path && reason == "cannot edit while background save is in progress"
    ));
    assert!(tab.text().starts_with("123abc\ndef\n"));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = tab.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "async save guard timed out");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!tab.is_busy());
    assert!(!tab.is_dirty());
    assert!(tab.text().starts_with("123abc\ndef\n"));
    assert!(fs::read(&path).unwrap().starts_with(b"123abc\ndef\n"));

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_repeated_save_as_async_keeps_first_job_authoritative() {
    let dir = std::env::temp_dir().join(format!("qem-editor-save-replace-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("source.txt");
    let output_a = dir.join("a.txt");
    let output_b = dir.join("b.txt");
    let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
    fs::write(&path, &original).unwrap();

    let mut tab = EditorTab::new(18);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.try_insert(TextPosition::new(0, 0), "123").unwrap();

    assert!(tab.save_as_async(output_a.clone()).unwrap());
    let err = tab.save_as_async(output_b.clone()).unwrap_err();
    assert!(matches!(
        err,
        crate::DocumentError::Write {
            path,
            ref source,
        } if path == output_b && source.to_string() == "save already in progress"
    ));
    assert_eq!(
        tab.save_state()
            .expect("first save job should remain authoritative")
            .path(),
        output_a.as_path()
    );
    assert!(matches!(
        tab.background_activity(),
        BackgroundActivity::Saving(_)
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = tab.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "repeated async save replacement test timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(tab.current_path(), Some(output_a.as_path()));
    assert!(fs::read(&output_a).unwrap().starts_with(b"123abc\ndef\n"));
    assert!(!output_b.exists());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&output_a);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_close_file_while_async_save_defers_until_completion() {
    let dir = std::env::temp_dir().join(format!("qem-editor-close-save-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("close-save.txt");
    let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
    fs::write(&path, &original).unwrap();

    let mut tab = EditorTab::new(17);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.try_insert(TextPosition::new(0, 0), "123").unwrap();
    let generation = tab.generation();
    let cursor = tab.cursor();

    assert!(tab.save_async().unwrap());
    tab.close_file();

    assert!(tab.is_saving());
    assert!(tab.is_busy());
    assert!(tab.close_pending());
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert_eq!(tab.cursor(), cursor);
    assert!(tab.text().starts_with("123abc\ndef\n"));
    assert_eq!(tab.generation(), generation);
    let status = tab.status();
    assert!(status.close_pending());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = tab.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "deferred close after save timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!tab.is_busy());
    assert!(!tab.close_pending());
    assert_eq!(tab.current_path(), None);
    assert_eq!(tab.text(), "");
    assert!(!tab.is_dirty());
    assert_eq!(tab.cursor(), CursorPosition::default());
    assert_eq!(tab.generation(), generation.wrapping_add(1));
    assert!(fs::read(&path).unwrap().starts_with(b"123abc\ndef\n"));
    assert!(!tab.status().close_pending());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_close_file_while_async_save_failure_keeps_dirty_document_open() {
    let dir = std::env::temp_dir().join(format!(
        "qem-editor-close-save-failure-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("close-save-failure.txt");
    let missing_parent = dir.join("missing");
    let output = missing_parent.join("copy.txt");
    fs::write(&path, b"abc\ndef\n").unwrap();

    let mut tab = EditorTab::new(19);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.try_insert(TextPosition::new(0, 0), "123").unwrap();
    let generation = tab.generation();
    let cursor = tab.cursor();
    let text = tab.text();

    assert!(tab.save_as_async(output.clone()).unwrap());
    tab.close_file();

    assert!(tab.is_saving());
    assert!(tab.is_busy());
    assert!(tab.close_pending());
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert_eq!(tab.cursor(), cursor);
    assert_eq!(tab.text(), text);
    assert!(tab.status().close_pending());

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = tab.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "deferred close after failed save timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(
        matches!(err, crate::DocumentError::Write { path: failed_path, .. } if failed_path == output)
    );
    assert!(!tab.is_busy());
    assert!(!tab.close_pending());
    assert!(tab.is_dirty());
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert_eq!(tab.cursor(), cursor);
    assert_eq!(tab.text(), text);
    assert_eq!(tab.generation(), generation);
    assert!(tab.save_state().is_none());
    assert!(!tab.status().close_pending());
    assert_eq!(fs::read(&path).unwrap(), b"abc\ndef\n");

    tab.close_file();
    assert_eq!(tab.current_path(), None);
    assert_eq!(tab.text(), "");
    assert!(!tab.is_dirty());
    assert_eq!(tab.cursor(), CursorPosition::default());
    assert_eq!(tab.generation(), generation.wrapping_add(1));

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_document_mut_while_close_is_deferred_cancels_pending_close() {
    let dir = std::env::temp_dir().join(format!(
        "qem-editor-close-save-raw-mutate-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("source.txt");
    let output = dir.join("output.txt");
    let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
    fs::write(&path, &original).unwrap();

    let mut tab = EditorTab::new(22);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.try_insert(TextPosition::new(0, 0), "123").unwrap();

    assert!(tab.save_as_async(output.clone()).unwrap());
    tab.close_file();
    assert!(tab.close_pending());
    assert!(tab.status().close_pending());

    let _ = tab
        .document_mut()
        .try_insert_text_at(0, 0, "Z")
        .expect("raw mutation should still be allowed");
    assert!(!tab.close_pending());
    assert!(!tab.status().close_pending());

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = tab.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "deferred close cancellation after raw mutation timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(
        err,
        crate::DocumentError::Write {
            path: failed_path,
            ref source,
        } if failed_path == output
            && source.to_string() == "background save result discarded after current session state changed"
    ));
    assert!(!tab.is_busy());
    assert!(!tab.close_pending());
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert!(tab.is_dirty());
    assert!(tab.text().starts_with("Z123abc\ndef\n"));

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&output);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_document_mut_during_async_save_discards_stale_save_result() {
    let dir =
        std::env::temp_dir().join(format!("qem-editor-raw-mutate-save-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("source.txt");
    let output = dir.join("output.txt");
    let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
    fs::write(&path, &original).unwrap();

    let mut tab = EditorTab::new(20);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.try_insert(TextPosition::new(0, 0), "123").unwrap();

    assert!(tab.save_as_async(output.clone()).unwrap());
    let _ = tab
        .document_mut()
        .try_insert_text_at(0, 0, "Z")
        .expect("raw document edit should still work");

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = tab.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "raw mutation during async save timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(
        err,
        crate::DocumentError::Write {
            path: failed_path,
            ref source,
        } if failed_path == output
            && source.to_string() == "background save result discarded after current session state changed"
    ));
    assert!(!tab.is_busy());
    assert!(tab.is_dirty());
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert!(tab.text().starts_with("Z123abc\ndef\n"));
    assert!(fs::read(&output).unwrap().starts_with(b"123abc\ndef\n"));
    assert_eq!(fs::read(&path).unwrap(), original);
    let issue = tab
        .background_issue()
        .expect("discarded background save should be retained");
    assert_eq!(issue.kind(), BackgroundIssueKind::SaveDiscarded);
    assert_eq!(issue.path(), output.as_path());
    assert_eq!(
        issue.message(),
        "background save result discarded after current session state changed"
    );
    let status = tab.status();
    let status_issue = status
        .background_issue()
        .expect("status should retain discarded background save");
    assert_eq!(status_issue.kind(), BackgroundIssueKind::SaveDiscarded);
    assert_eq!(status_issue.path(), output.as_path());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&output);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn update_cursor_char_index_treats_crlf_as_single_newline() {
    let dir = std::env::temp_dir().join(format!("qem-editor-crlf-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("crlf.txt");
    fs::write(&path, b"a\r\nb\r\n").unwrap();

    let mut tab = EditorTab::new(1);
    tab.open_file(path.clone()).unwrap();

    tab.update_cursor_char_index(2);
    assert_eq!(tab.cursor().line(), 2);
    assert_eq!(tab.cursor().column(), 1);

    tab.update_cursor_char_index(3);
    assert_eq!(tab.cursor().line(), 2);
    assert_eq!(tab.cursor().column(), 1);

    tab.update_cursor_char_index(4);
    assert_eq!(tab.cursor().line(), 2);
    assert_eq!(tab.cursor().column(), 2);

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cursor_position_roundtrips_with_text_position() {
    let cursor = CursorPosition::new(3, 5);
    let position = cursor.to_text_position();

    assert_eq!(position.line0(), 2);
    assert_eq!(position.col0(), 4);
    assert_eq!(CursorPosition::from_text_position(position), cursor);
}

#[test]
fn open_file_async_completes_and_exposes_progress() {
    let dir = std::env::temp_dir().join(format!("qem-editor-open-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("open.txt");
    fs::write(&path, b"alpha\nbeta\n").unwrap();

    let mut tab = EditorTab::new(7);
    tab.set_cursor_line_col(9, 9);
    tab.open_file_async(path.clone()).unwrap();

    let progress = tab
        .loading_state()
        .expect("typed load progress should exist");
    assert_eq!(progress.total_bytes(), fs::metadata(&path).unwrap().len());
    assert_eq!(progress.path(), path.as_path());
    let typed_progress = tab
        .loading_state()
        .expect("typed load progress should exist");
    assert!(typed_progress.completed_bytes() <= typed_progress.total_bytes());
    assert_eq!(typed_progress.total_bytes(), progress.total_bytes());
    assert!(typed_progress.load_phase().is_some());
    assert!(tab.loading_phase().is_some());
    assert!(matches!(
        tab.background_activity(),
        BackgroundActivity::Loading(_)
    ));
    let loading_status = tab.status();
    assert!(loading_status.is_loading());
    assert!(loading_status.loading_phase().is_some());
    assert_eq!(
        loading_status
            .loading_state()
            .expect("status should expose typed loading progress")
            .path(),
        path.as_path()
    );
    assert!(tab.is_loading());
    assert!(tab.is_busy());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = tab.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "async load timed out");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!tab.is_loading());
    assert_eq!(tab.cursor().line(), 1);
    assert_eq!(tab.cursor().column(), 1);
    assert_eq!(tab.cursor_position().line0(), 0);
    assert_eq!(tab.cursor_position().col0(), 0);
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert_eq!(tab.exact_line_count(), Some(3));
    assert_eq!(tab.display_line_count(), 3);
    assert!(tab.is_line_count_exact());
    assert_eq!(tab.line_len_chars(0), 5);
    assert_eq!(tab.position_for_char_index(6), TextPosition::new(1, 0));
    assert_eq!(tab.char_index_for_position(TextPosition::new(1, 0)), 6);
    assert_eq!(
        tab.text_range_between(TextPosition::new(1, 2), TextPosition::new(0, 4))
            .len_chars(),
        4
    );
    let viewport = tab.read_viewport(ViewportRequest::new(0, 2).with_columns(0, 16));
    assert_eq!(viewport.rows()[0].text(), "alpha");
    assert!(matches!(
        tab.background_activity(),
        BackgroundActivity::Idle
    ));
    assert!(tab.background_issue().is_none());
    assert!(loading_status.background_issue().is_none());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn open_file_async_failure_preserves_existing_tab_state() {
    let dir = std::env::temp_dir().join(format!("qem-editor-open-failure-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("open.txt");
    let missing = dir.join("missing.txt");
    fs::write(&path, b"alpha\nbeta\n").unwrap();

    let mut tab = EditorTab::new(8);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.document_mut().try_insert_text_at(0, 0, "X").unwrap();
    tab.set_cursor_position(TextPosition::new(1, 2));

    let generation = tab.generation();
    let cursor = tab.cursor();
    let text = tab.text();

    tab.open_file_async(missing.clone()).unwrap();

    let progress = tab
        .loading_state()
        .expect("failed load should still expose typed progress while running");
    assert_eq!(progress.path(), missing.as_path());
    assert_eq!(progress.total_bytes(), 0);
    assert!(tab.is_loading());
    assert!(tab.is_busy());

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = tab.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(Instant::now() < deadline, "async load failure timed out");
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(
        err,
        crate::DocumentError::Open { path, .. } if path == missing
    ));
    assert!(!tab.is_loading());
    assert!(!tab.is_busy());
    assert!(tab.loading_state().is_none());
    assert!(matches!(
        tab.background_activity(),
        BackgroundActivity::Idle
    ));
    assert_eq!(tab.generation(), generation);
    assert_eq!(tab.cursor(), cursor);
    assert_eq!(tab.current_path(), Some(path.as_path()));
    assert_eq!(tab.text(), text);
    assert!(tab.is_dirty());
    let issue = tab
        .background_issue()
        .expect("failed load should retain background issue");
    assert_eq!(issue.kind(), BackgroundIssueKind::LoadFailed);
    assert_eq!(issue.path(), missing.as_path());
    let status = tab.status();
    let status_issue = status
        .background_issue()
        .expect("status should retain failed load");
    assert_eq!(status_issue.kind(), BackgroundIssueKind::LoadFailed);
    assert_eq!(status_issue.path(), missing.as_path());

    tab.open_file(path.clone()).unwrap();
    assert!(tab.background_issue().is_none());
    assert!(tab.status().background_issue().is_none());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_take_background_issue_clears_retained_failure() {
    let dir = std::env::temp_dir().join(format!("qem-editor-ack-load-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let missing = dir.join("missing.txt");

    let mut tab = EditorTab::new(23);
    tab.open_file_async(missing.clone()).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = tab.poll_background_job() {
            let err = result.unwrap_err();
            assert!(matches!(
                err,
                crate::DocumentError::Open { path, .. } if path == missing
            ));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background load failure timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let issue = tab
        .take_background_issue()
        .expect("failed load should be available for explicit acknowledgement");
    assert_eq!(issue.kind(), BackgroundIssueKind::LoadFailed);
    assert_eq!(issue.path(), missing.as_path());
    assert!(tab.background_issue().is_none());
    assert!(tab.status().background_issue().is_none());
    assert!(tab.take_background_issue().is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_repeated_open_file_async_keeps_first_job_authoritative() {
    let dir = std::env::temp_dir().join(format!("qem-session-open-replace-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let first = dir.join("first.txt");
    let second = dir.join("second.txt");
    fs::write(&first, b"first\n").unwrap();
    fs::write(&second, b"second\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file_async(first.clone()).unwrap();

    let err = session.open_file_async(second.clone()).unwrap_err();
    assert!(matches!(
        err,
        crate::DocumentError::Open {
            path,
            ref source,
        } if path == second && source.to_string() == "load already in progress"
    ));
    assert_eq!(
        session
            .loading_state()
            .expect("first load job should remain authoritative")
            .path(),
        first.as_path()
    );
    assert!(matches!(
        session.background_activity(),
        BackgroundActivity::Loading(_)
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = session.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "repeated async open replacement test timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(session.current_path(), Some(first.as_path()));
    assert_eq!(session.text(), "first\n");

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_try_insert_updates_cursor() {
    let mut tab = EditorTab::new(11);

    let cursor = tab
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();
    let status = tab.status();

    assert_eq!(cursor, TextPosition::new(1, 4));
    assert_eq!(tab.cursor_position(), TextPosition::new(1, 4));
    assert_eq!(tab.cursor().line(), 2);
    assert_eq!(tab.cursor().column(), 5);
    assert!(tab.is_dirty());
    assert_eq!(tab.display_line_count(), 2);
    assert_eq!(tab.text(), "alpha\nbeta");
    assert_eq!(status.id(), 11);
    assert_eq!(status.generation(), 0);
    assert_eq!(status.cursor().line(), 2);
    assert_eq!(status.cursor().column(), 5);
    assert!(status.is_dirty());
    assert_eq!(status.file_len(), tab.file_len());
    assert_eq!(status.exact_line_count(), Some(2));
    assert!(status.has_edit_buffer());
    assert!(status.has_rope());
    assert!(!status.has_piece_table());
    assert!(!status.is_busy());
}

#[test]
fn editor_tab_selection_helpers_update_cursor() {
    let mut tab = EditorTab::new(12);
    let _ = tab
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    assert_eq!(
        tab.edit_capability_at(TextPosition::new(0, 1)),
        EditCapability::Editable {
            backing: DocumentBacking::Rope,
        }
    );
    let selection = TextSelection::new(TextPosition::new(1, 2), TextPosition::new(0, 4));
    let selected = tab.read_selection(selection);
    assert!(selected.is_exact());
    assert_eq!(selected.text(), "a\nbe");
    let cursor = tab.try_replace_selection(selection, "Z").unwrap();

    assert_eq!(cursor, TextPosition::new(0, 5));
    assert_eq!(tab.cursor_position(), TextPosition::new(0, 5));
    assert_eq!(tab.text(), "alphZta");

    let delete = tab
        .try_delete_selection(TextSelection::caret(TextPosition::new(0, 2)))
        .unwrap();
    assert!(!delete.changed());
    assert_eq!(delete.cursor(), TextPosition::new(0, 2));
    assert_eq!(tab.cursor_position(), TextPosition::new(0, 2));
}

#[test]
fn editor_tab_delete_forward_updates_cursor() {
    let mut tab = EditorTab::new(13);
    let _ = tab
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let result = tab.try_delete_forward(TextPosition::new(0, 5)).unwrap();
    assert!(result.changed());
    assert_eq!(result.cursor(), TextPosition::new(0, 5));
    assert_eq!(tab.cursor_position(), TextPosition::new(0, 5));
    assert_eq!(tab.text(), "alphabeta");
}

#[test]
fn editor_tab_cut_selection_updates_cursor() {
    let mut tab = EditorTab::new(14);
    let _ = tab
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let cut = tab
        .try_cut_selection(TextSelection::new(
            TextPosition::new(0, 3),
            TextPosition::new(1, 2),
        ))
        .unwrap();

    assert_eq!(cut.text(), "ha\nbe");
    assert!(cut.changed());
    assert_eq!(cut.cursor(), TextPosition::new(0, 3));
    assert_eq!(tab.cursor_position(), TextPosition::new(0, 3));
    assert_eq!(tab.text(), "alpta");
}

#[test]
fn editor_tab_selection_delete_commands_update_cursor() {
    let mut tab = EditorTab::new(15);
    let _ = tab
        .try_insert(TextPosition::new(0, 0), "alpha\nbeta")
        .unwrap();

    let deleted = tab
        .try_delete_forward_selection(TextSelection::new(
            TextPosition::new(0, 3),
            TextPosition::new(1, 2),
        ))
        .unwrap();
    assert!(deleted.changed());
    assert_eq!(deleted.cursor(), TextPosition::new(0, 3));
    assert_eq!(tab.cursor_position(), TextPosition::new(0, 3));
    assert_eq!(tab.text(), "alpta");

    let backspace = tab
        .try_backspace_selection(TextSelection::caret(TextPosition::new(0, 2)))
        .unwrap();
    assert!(backspace.changed());
    assert_eq!(backspace.cursor(), TextPosition::new(0, 1));
    assert_eq!(tab.cursor_position(), TextPosition::new(0, 1));
    assert_eq!(tab.text(), "apta");
}

#[test]
fn cancel_clear_dirty_after_open_preserves_real_edit() {
    let dir = std::env::temp_dir().join(format!("qem-editor-dirty-open-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("dirty-open.txt");
    fs::write(&path, b"alpha\n").unwrap();

    let mut tab = EditorTab::new(3);
    tab.open_file(path.clone()).unwrap();
    let _ = tab.document_mut().try_insert_text_at(0, 0, "X").unwrap();
    tab.cancel_clear_dirty_after_open();
    tab.after_text_edit_frame();

    assert!(tab.is_dirty());
    assert!(tab.text().starts_with('X'));

    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_rejects_edits_while_async_open_is_in_progress() {
    let dir = std::env::temp_dir().join(format!("qem-session-open-guard-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let current = dir.join("current.txt");
    let next = dir.join("next.txt");
    fs::write(&current, b"current\n").unwrap();
    fs::write(&next, b"next\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file(current.clone()).unwrap();
    assert_eq!(session.text(), "current\n");

    session.open_file_async(next.clone()).unwrap();
    assert!(session.is_loading());

    let err = session
        .try_insert(TextPosition::new(0, 0), "X")
        .unwrap_err();
    assert!(matches!(
        err,
        crate::DocumentError::EditUnsupported {
            path: Some(ref blocked_path),
            reason,
        } if blocked_path == &current && reason == "cannot edit while background load is in progress"
    ));
    assert_eq!(session.current_path(), Some(current.as_path()));
    assert_eq!(session.text(), "current\n");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = session.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "async open guard timed out");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!session.is_busy());
    assert_eq!(session.current_path(), Some(next.as_path()));
    assert_eq!(session.text(), "next\n");

    let _ = fs::remove_file(&current);
    let _ = fs::remove_file(&next);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_save_apis_report_loading_instead_of_no_path_during_first_async_open() {
    let dir = std::env::temp_dir().join(format!(
        "qem-session-save-during-open-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let input = dir.join("input.txt");
    fs::write(&input, b"input\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file_async(input.clone()).unwrap();
    assert!(session.is_loading());
    assert!(session.current_path().is_none());

    let save_err = session.save().unwrap_err();
    assert!(matches!(
        save_err,
        SaveError::Io(crate::DocumentError::Write {
            path,
            ref source,
        }) if path == input && source.to_string() == "cannot save while load is in progress"
    ));

    let save_async_err = session.save_async().unwrap_err();
    assert!(matches!(
        save_async_err,
        SaveError::Io(crate::DocumentError::Write {
            path,
            ref source,
        }) if path == input && source.to_string() == "cannot save while load is in progress"
    ));

    assert_eq!(
        session
            .loading_state()
            .expect("load job should remain active after rejected saves")
            .path(),
        input.as_path()
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = session.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "save during async open test timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(session.current_path(), Some(input.as_path()));
    assert_eq!(session.text(), "input\n");

    let _ = fs::remove_file(&input);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_document_mut_during_async_open_discards_stale_load_result() {
    let dir = std::env::temp_dir().join(format!(
        "qem-session-raw-mutate-open-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let current = dir.join("current.txt");
    let next = dir.join("next.txt");
    fs::write(&current, b"current\n").unwrap();
    fs::write(&next, b"next\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file(current.clone()).unwrap();
    session.open_file_async(next.clone()).unwrap();
    let _ = session
        .document_mut()
        .try_insert_text_at(0, 0, "X")
        .expect("raw document edit should still work");

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = session.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "raw mutation during async open timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(
        err,
        crate::DocumentError::Open {
            path: failed_path,
            ref source,
        } if failed_path == next
            && source.to_string() == "background load result discarded after current session state changed"
    ));
    assert!(!session.is_busy());
    assert_eq!(session.current_path(), Some(current.as_path()));
    assert!(session.is_dirty());
    assert_eq!(session.text(), "Xcurrent\n");
    let issue = session
        .background_issue()
        .expect("discarded background load should be retained");
    assert_eq!(issue.kind(), BackgroundIssueKind::LoadDiscarded);
    assert_eq!(issue.path(), next.as_path());
    assert_eq!(
        issue.message(),
        "background load result discarded after current session state changed"
    );
    let status = session.status();
    let status_issue = status
        .background_issue()
        .expect("status should retain discarded background load");
    assert_eq!(status_issue.kind(), BackgroundIssueKind::LoadDiscarded);
    assert_eq!(status_issue.path(), next.as_path());

    let _ = fs::remove_file(&current);
    let _ = fs::remove_file(&next);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_take_background_issue_clears_retained_discard() {
    let dir = std::env::temp_dir().join(format!("qem-session-ack-discard-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let current = dir.join("current.txt");
    let next = dir.join("next.txt");
    fs::write(&current, b"current\n").unwrap();
    fs::write(&next, b"next\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file(current.clone()).unwrap();
    session.open_file_async(next.clone()).unwrap();
    session
        .document_mut()
        .try_insert_text_at(0, 0, "X")
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = session.poll_background_job() {
            let err = result.unwrap_err();
            assert!(matches!(
                err,
                crate::DocumentError::Open { path, ref source }
                    if path == next
                        && source.to_string()
                            == "background load result discarded after current session state changed"
            ));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "discarded background load timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let issue = session
        .take_background_issue()
        .expect("discarded load should be available for explicit acknowledgement");
    assert_eq!(issue.kind(), BackgroundIssueKind::LoadDiscarded);
    assert_eq!(issue.path(), next.as_path());
    assert!(session.background_issue().is_none());
    assert!(session.status().background_issue().is_none());
    assert!(session.take_background_issue().is_none());

    let _ = fs::remove_file(&current);
    let _ = fs::remove_file(&next);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn editor_tab_set_path_during_async_save_discards_stale_save_result() {
    let dir = std::env::temp_dir().join(format!("qem-editor-path-save-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let source = dir.join("source.txt");
    let output = dir.join("output.txt");
    let override_path = dir.join("override.txt");
    let original = b"abc\ndef\n".repeat((1024 * 1024 / 8) + 1);
    fs::write(&source, &original).unwrap();

    let mut tab = EditorTab::new(21);
    tab.open_file(source.clone()).unwrap();
    let _ = tab.try_insert(TextPosition::new(0, 0), "123").unwrap();

    assert!(tab.save_as_async(output.clone()).unwrap());
    tab.set_path(override_path.clone());

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = tab.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "path override during async save timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(
        err,
        crate::DocumentError::Write {
            path: failed_path,
            ref source,
        } if failed_path == output
            && source.to_string() == "background save result discarded after current session state changed"
    ));
    assert!(!tab.is_busy());
    assert!(tab.is_dirty());
    assert_eq!(tab.current_path(), Some(override_path.as_path()));
    assert!(tab.text().starts_with("123abc\ndef\n"));
    assert!(fs::read(&output).unwrap().starts_with(b"123abc\ndef\n"));
    assert_eq!(fs::read(&source).unwrap(), original);
    let issue = tab
        .background_issue()
        .expect("discarded background save should be retained");
    assert_eq!(issue.kind(), BackgroundIssueKind::SaveDiscarded);
    assert_eq!(issue.path(), output.as_path());

    let _ = fs::remove_file(&source);
    let _ = fs::remove_file(&output);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_set_path_during_async_open_discards_stale_load_result() {
    let dir = std::env::temp_dir().join(format!("qem-session-path-open-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let current = dir.join("current.txt");
    let next = dir.join("next.txt");
    let override_path = dir.join("override.txt");
    fs::write(&current, b"current\n").unwrap();
    fs::write(&next, b"next\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file(current.clone()).unwrap();
    session.open_file_async(next.clone()).unwrap();
    session.set_path(override_path.clone());

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = session.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "path override during async open timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(
        err,
        crate::DocumentError::Open {
            path: failed_path,
            ref source,
        } if failed_path == next
            && source.to_string() == "background load result discarded after current session state changed"
    ));
    assert!(!session.is_busy());
    assert_eq!(session.current_path(), Some(override_path.as_path()));
    assert!(!session.is_dirty());
    assert_eq!(session.text(), "current\n");
    let issue = session
        .background_issue()
        .expect("discarded background load should be retained");
    assert_eq!(issue.kind(), BackgroundIssueKind::LoadDiscarded);
    assert_eq!(issue.path(), next.as_path());

    let _ = fs::remove_file(&current);
    let _ = fs::remove_file(&next);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_set_path_while_close_is_deferred_cancels_pending_close() {
    let dir = std::env::temp_dir().join(format!(
        "qem-session-close-open-set-path-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let current = dir.join("current.txt");
    let next = dir.join("next.txt");
    let override_path = dir.join("override.txt");
    fs::write(&current, b"current\n").unwrap();
    fs::write(&next, b"next\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file(current.clone()).unwrap();
    session.open_file_async(next.clone()).unwrap();
    session.close_file();
    assert!(session.close_pending());
    assert!(session.status().close_pending());

    session.set_path(override_path.clone());
    assert!(!session.close_pending());
    assert!(!session.status().close_pending());

    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        if let Some(result) = session.poll_background_job() {
            break result.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "deferred close cancellation after set_path timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(matches!(
        err,
        crate::DocumentError::Open {
            path: failed_path,
            ref source,
        } if failed_path == next
            && source.to_string() == "background load result discarded after current session state changed"
    ));
    assert!(!session.is_busy());
    assert!(!session.close_pending());
    assert_eq!(session.current_path(), Some(override_path.as_path()));
    assert_eq!(session.text(), "current\n");

    let _ = fs::remove_file(&current);
    let _ = fs::remove_file(&next);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_close_file_while_async_open_defers_until_completion() {
    let dir = std::env::temp_dir().join(format!("qem-session-close-open-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let current = dir.join("current.txt");
    let next = dir.join("next.txt");
    fs::write(&current, b"current\n").unwrap();
    fs::write(&next, b"next\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file(current.clone()).unwrap();
    let generation = session.generation();

    session.open_file_async(next.clone()).unwrap();
    session.close_file();

    assert!(session.is_loading());
    assert!(session.is_busy());
    assert!(session.close_pending());
    assert_eq!(session.current_path(), Some(current.as_path()));
    assert_eq!(session.text(), "current\n");
    assert_eq!(session.generation(), generation);
    assert!(session.status().close_pending());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = session.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "deferred close after async open timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!session.is_busy());
    assert!(!session.close_pending());
    assert_eq!(session.current_path(), None);
    assert_eq!(session.text(), "");
    assert!(!session.is_dirty());
    assert_eq!(session.generation(), generation.wrapping_add(1));
    assert!(!session.status().close_pending());

    let _ = fs::remove_file(&current);
    let _ = fs::remove_file(&next);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn document_session_open_save_and_viewport_flow() {
    let dir = std::env::temp_dir().join(format!("qem-session-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let input = dir.join("input.txt");
    let output = dir.join("output.txt");
    fs::write(&input, b"alpha\nbeta\n").unwrap();

    let mut session = DocumentSession::new();
    session.open_file_async(input.clone()).unwrap();

    let loading = session
        .loading_state()
        .expect("session should expose loading progress");
    assert_eq!(loading.path(), input.as_path());
    assert_eq!(loading.total_bytes(), fs::metadata(&input).unwrap().len());
    assert!(loading.load_phase().is_some());
    assert!(session.loading_phase().is_some());
    assert!(session.status().loading_phase().is_some());
    assert!(matches!(
        session.loading_phase(),
        Some(
            LoadPhase::Opening
                | LoadPhase::InspectingSource
                | LoadPhase::PreparingIndex
                | LoadPhase::RecoveringSession
                | LoadPhase::Ready
        )
    ));
    assert!(matches!(
        session.background_activity(),
        BackgroundActivity::Loading(_)
    ));
    assert!(session.status().is_loading());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = session.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "document session load timed out");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(session.current_path(), Some(input.as_path()));
    let status = session.status();
    assert_eq!(status.generation(), session.generation());
    assert_eq!(status.path(), Some(input.as_path()));
    assert!(!status.is_dirty());
    assert_eq!(status.display_line_count(), 3);
    assert_eq!(status.exact_line_count(), Some(3));
    assert_eq!(status.file_len(), session.file_len());
    assert_eq!(status.line_ending(), session.line_ending());
    assert!(!status.is_busy());
    assert_eq!(
        session.file_len(),
        fs::metadata(&input).unwrap().len() as usize
    );
    assert_eq!(session.exact_line_count(), Some(3));
    assert_eq!(session.display_line_count(), 3);
    assert!(session.is_line_count_exact());
    assert_eq!(session.line_len_chars(1), 4);
    assert_eq!(session.position_for_char_index(6), TextPosition::new(1, 0));
    assert_eq!(session.char_index_for_position(TextPosition::new(1, 0)), 6);
    assert_eq!(
        session.text_units_between(TextPosition::new(0, 4), TextPosition::new(1, 2)),
        4
    );
    assert_eq!(
        session.edit_capability_at(TextPosition::new(0, 1)),
        EditCapability::RequiresPromotion {
            from: DocumentBacking::Mmap,
            to: DocumentBacking::Rope,
        }
    );
    let selection = TextSelection::new(TextPosition::new(1, 2), TextPosition::new(0, 4));
    let selected = session.read_selection(selection);
    assert!(selected.is_exact());
    assert_eq!(selected.text(), "a\nbe");
    let viewport = session.read_viewport(ViewportRequest::new(0, 2).with_columns(0, 16));
    assert_eq!(viewport.rows()[0].text(), "alpha");
    assert_eq!(viewport.rows()[1].text(), "beta");

    let cursor = session.try_replace_selection(selection, "Z").unwrap();
    assert_eq!(cursor, TextPosition::new(0, 5));
    assert_eq!(session.text(), "alphZta\n");

    let delete = session.try_delete_forward(TextPosition::new(0, 5)).unwrap();
    assert!(delete.changed());
    assert_eq!(delete.cursor(), TextPosition::new(0, 5));
    assert_eq!(session.text(), "alphZa\n");

    let cut = session
        .try_cut_selection(TextSelection::new(
            TextPosition::new(0, 4),
            TextPosition::new(0, 6),
        ))
        .unwrap();
    assert_eq!(cut.text(), "Za");
    assert!(cut.changed());
    assert_eq!(cut.cursor(), TextPosition::new(0, 4));
    assert_eq!(session.text(), "alph\n");

    let deleted = session
        .try_delete_forward_selection(TextSelection::new(
            TextPosition::new(0, 1),
            TextPosition::new(0, 3),
        ))
        .unwrap();
    assert!(deleted.changed());
    assert_eq!(deleted.cursor(), TextPosition::new(0, 1));
    assert_eq!(session.text(), "ah\n");

    let backspace = session
        .try_backspace_selection(TextSelection::caret(TextPosition::new(0, 1)))
        .unwrap();
    assert!(backspace.changed());
    assert_eq!(backspace.cursor(), TextPosition::new(0, 0));
    assert_eq!(session.text(), "h\n");

    let _ = session
        .try_insert(TextPosition::new(0, 0), "// inserted by session\n")
        .unwrap();
    assert!(session.is_dirty());
    assert!(session.save_as_async(output.clone()).unwrap());

    let saving = session
        .save_state()
        .expect("session should expose save progress");
    assert_eq!(saving.path(), output.as_path());
    assert!(matches!(
        session.background_activity(),
        BackgroundActivity::Saving(_)
    ));
    let saving_status = session.status();
    assert!(saving_status.is_saving());
    assert_eq!(
        saving_status
            .save_state()
            .expect("status should expose typed save progress")
            .path(),
        output.as_path()
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = session.poll_background_job() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "document session save timed out");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!session.is_dirty());
    assert!(matches!(
        session.background_activity(),
        BackgroundActivity::Idle
    ));
    assert_eq!(session.current_path(), Some(output.as_path()));
    assert!(fs::read_to_string(&output)
        .unwrap()
        .starts_with("// inserted by session\nh\n"));

    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    let _ = fs::remove_dir_all(&dir);
}

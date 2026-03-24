use super::*;

#[derive(Debug)]
struct SaveJob {
    path: Arc<PathBuf>,
    total_bytes: u64,
    written_bytes: Arc<AtomicU64>,
    rx: mpsc::Receiver<Result<SaveCompletion, DocumentError>>,
}

#[derive(Debug)]
struct LoadJob {
    path: Arc<PathBuf>,
    total_bytes: u64,
    loaded_bytes: Arc<AtomicU64>,
    phase: Arc<AtomicU8>,
    rx: mpsc::Receiver<Result<Document, DocumentError>>,
}

#[derive(Debug)]
pub(crate) struct SessionCore {
    doc: Document,
    generation: u64,
    load_job: Option<LoadJob>,
    save_job: Option<SaveJob>,
    clear_dirty_after_open: bool,
    close_after_job: bool,
    discard_load_result: bool,
    discard_save_result: bool,
    last_background_issue: Option<BackgroundIssue>,
}

impl SessionCore {
    pub(super) fn new() -> Self {
        Self {
            doc: Document::new(),
            generation: 0,
            load_job: None,
            save_job: None,
            clear_dirty_after_open: false,
            close_after_job: false,
            discard_load_result: false,
            discard_save_result: false,
            last_background_issue: None,
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn is_saving(&self) -> bool {
        self.save_job.is_some()
    }

    pub(super) fn is_loading(&self) -> bool {
        self.load_job.is_some()
    }

    pub(super) fn is_busy(&self) -> bool {
        self.is_loading() || self.is_saving()
    }

    pub(super) fn indexing_progress(&self) -> Option<(usize, usize)> {
        self.doc
            .indexing_state()
            .map(|progress| (progress.completed_bytes(), progress.total_bytes()))
    }

    pub(super) fn indexing_state(&self) -> Option<ByteProgress> {
        self.doc.indexing_state()
    }

    pub(super) fn loading_state(&self) -> Option<FileProgress> {
        let job = self.load_job.as_ref()?;
        Some(FileProgress::loading(
            Arc::clone(&job.path),
            job.loaded_bytes
                .load(Ordering::Relaxed)
                .min(job.total_bytes),
            job.total_bytes,
            LoadPhase::from_raw(job.phase.load(Ordering::Relaxed)),
        ))
    }

    pub(super) fn loading_phase(&self) -> Option<LoadPhase> {
        self.loading_state()
            .and_then(|progress| progress.load_phase())
    }

    pub(super) fn loading_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.loading_state().map(|progress| {
            (
                progress.completed_bytes(),
                progress.total_bytes(),
                progress.path().to_path_buf(),
            )
        })
    }

    pub(super) fn poll_load_job(&mut self) -> Option<Result<(), DocumentError>> {
        let state = match self.load_job.as_ref()?.rx.try_recv() {
            Ok(res) => res,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                let Some(job) = self.load_job.take() else {
                    let err = missing_load_job_error();
                    self.last_background_issue = Some(background_issue_from_error(
                        BackgroundIssueKind::LoadFailed,
                        &err,
                    ));
                    return Some(Err(err));
                };
                let err = DocumentError::Open {
                    path: Arc::unwrap_or_clone(job.path),
                    source: io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "load worker disconnected unexpectedly",
                    ),
                };
                self.last_background_issue = Some(background_issue_from_error(
                    BackgroundIssueKind::LoadFailed,
                    &err,
                ));
                return Some(Err(err));
            }
        };

        let Some(job) = self.load_job.take() else {
            let err = missing_load_job_error();
            self.last_background_issue = Some(background_issue_from_error(
                BackgroundIssueKind::LoadFailed,
                &err,
            ));
            return Some(Err(err));
        };
        let close_after_job = self.close_after_job;
        self.close_after_job = false;
        let discard_load_result = self.discard_load_result;
        self.discard_load_result = false;
        Some(match state {
            Ok(doc) => {
                if discard_load_result {
                    let err = DocumentError::Open {
                        path: Arc::unwrap_or_clone(job.path),
                        source: io::Error::other(
                            "background load result discarded after current session state changed",
                        ),
                    };
                    self.last_background_issue = Some(background_issue_from_error(
                        BackgroundIssueKind::LoadDiscarded,
                        &err,
                    ));
                    Err(err)
                } else if close_after_job {
                    self.last_background_issue = None;
                    self.finish_close();
                    Ok(())
                } else {
                    self.last_background_issue = None;
                    self.finish_open(doc);
                    Ok(())
                }
            }
            Err(err) => {
                self.last_background_issue = Some(background_issue_from_error(
                    BackgroundIssueKind::LoadFailed,
                    &err,
                ));
                if close_after_job && !discard_load_result {
                    self.finish_close();
                }
                Err(err)
            }
        })
    }

    pub(super) fn poll_background_job(&mut self) -> Option<Result<(), DocumentError>> {
        self.poll_load_job().or_else(|| self.poll_save_job())
    }

    pub(super) fn ensure_idle_for_edit(&self) -> Result<(), DocumentError> {
        let path = self.current_path().map(Path::to_path_buf);
        if self.is_loading() {
            return Err(DocumentError::EditUnsupported {
                path,
                reason: "cannot edit while background load is in progress",
            });
        }
        if self.is_saving() {
            return Err(DocumentError::EditUnsupported {
                path,
                reason: "cannot edit while background save is in progress",
            });
        }
        Ok(())
    }

    pub(super) fn document(&self) -> &Document {
        &self.doc
    }

    fn note_session_state_change(&mut self) {
        let mut changed_while_busy = false;
        if self.is_loading() {
            self.discard_load_result = true;
            changed_while_busy = true;
        }
        if self.is_saving() {
            self.discard_save_result = true;
            changed_while_busy = true;
        }
        if changed_while_busy {
            self.close_after_job = false;
        }
    }

    pub(super) fn document_mut(&mut self) -> &mut Document {
        self.note_session_state_change();
        &mut self.doc
    }

    fn active_load_path(&self) -> Option<&Path> {
        self.load_job.as_ref().map(|job| job.path.as_path())
    }

    fn load_report_path(&self) -> PathBuf {
        self.active_load_path()
            .map(Path::to_path_buf)
            .or_else(|| self.current_path().map(Path::to_path_buf))
            .unwrap_or_default()
    }

    pub(super) fn current_path(&self) -> Option<&Path> {
        self.doc.path()
    }

    pub(super) fn background_issue(&self) -> Option<&BackgroundIssue> {
        self.last_background_issue.as_ref()
    }

    pub(super) fn take_background_issue(&mut self) -> Option<BackgroundIssue> {
        self.last_background_issue.take()
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.doc.is_dirty()
    }

    pub(super) fn open_file(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot open while save is in progress"),
            });
        }
        if self.is_loading() {
            return Err(DocumentError::Open {
                path,
                source: io::Error::other("cannot open while another load is in progress"),
            });
        }
        let doc = Document::open(path)?;
        self.last_background_issue = None;
        self.finish_open(doc);
        Ok(())
    }

    pub(super) fn open_file_async(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot open while save is in progress"),
            });
        }
        if self.is_loading() {
            return Err(DocumentError::Open {
                path,
                source: io::Error::other("load already in progress"),
            });
        }

        let total_bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let loaded_bytes = Arc::new(AtomicU64::new(0));
        let phase = Arc::new(AtomicU8::new(LoadPhase::Opening.as_raw()));
        let job_path = Arc::new(path);
        let rx = spawn_load_worker(
            (*job_path).clone(),
            total_bytes,
            Arc::clone(&loaded_bytes),
            Arc::clone(&phase),
        );
        self.load_job = Some(LoadJob {
            path: job_path,
            total_bytes,
            loaded_bytes,
            phase,
            rx,
        });
        self.clear_dirty_after_open = false;
        self.last_background_issue = None;
        Ok(())
    }

    pub(super) fn close_file(&mut self) -> bool {
        self.clear_dirty_after_open = false;
        if self.is_busy() {
            self.close_after_job = true;
            return false;
        }
        self.finish_close();
        true
    }

    pub(super) fn close_pending(&self) -> bool {
        self.close_after_job
    }

    pub(super) fn is_empty_document(&self) -> bool {
        self.doc.path().is_none() && self.doc.file_len() == 0 && !self.doc.is_dirty()
    }

    fn finish_close(&mut self) {
        self.load_job = None;
        self.save_job = None;
        self.close_after_job = false;
        self.discard_load_result = false;
        self.discard_save_result = false;
        self.last_background_issue = None;
        self.doc = Document::new();
        self.generation = self.generation.wrapping_add(1);
        self.clear_dirty_after_open = false;
    }

    pub(super) fn after_document_frame(&mut self) {
        if !self.clear_dirty_after_open {
            return;
        }
        self.doc.mark_clean();
        self.clear_dirty_after_open = false;
    }

    pub(super) fn cancel_clear_dirty_after_open(&mut self) {
        self.clear_dirty_after_open = false;
    }

    pub(super) fn save(&mut self) -> Result<(), SaveError> {
        if self.is_loading() {
            let path = self.load_report_path();
            return Err(SaveError::Io(DocumentError::Write {
                path,
                source: io::Error::other("cannot save while load is in progress"),
            }));
        }
        let Some(path) = self.current_path().map(|p| p.to_path_buf()) else {
            return Err(SaveError::NoPath);
        };
        if !self.doc.is_dirty() {
            return Ok(());
        }
        self.doc.save_to(&path).map_err(SaveError::Io)?;
        self.last_background_issue = None;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    pub(super) fn save_as(&mut self, path: PathBuf) -> Result<(), DocumentError> {
        if self.is_loading() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot save while load is in progress"),
            });
        }
        self.doc.save_to(&path)?;
        self.last_background_issue = None;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    pub(super) fn set_path(&mut self, path: PathBuf) {
        self.note_session_state_change();
        self.doc.set_path(path);
    }

    pub(super) fn save_state(&self) -> Option<FileProgress> {
        let job = self.save_job.as_ref()?;
        Some(FileProgress::new(
            Arc::clone(&job.path),
            job.written_bytes
                .load(Ordering::Relaxed)
                .min(job.total_bytes),
            job.total_bytes,
        ))
    }

    pub(super) fn save_progress(&self) -> Option<(u64, u64, PathBuf)> {
        self.save_state().map(|progress| {
            (
                progress.completed_bytes(),
                progress.total_bytes(),
                progress.path().to_path_buf(),
            )
        })
    }

    pub(super) fn poll_save_job(&mut self) -> Option<Result<(), DocumentError>> {
        let state = match self.save_job.as_ref()?.rx.try_recv() {
            Ok(res) => res,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                let Some(job) = self.save_job.take() else {
                    let err = missing_save_job_error();
                    self.last_background_issue = Some(background_issue_from_error(
                        BackgroundIssueKind::SaveFailed,
                        &err,
                    ));
                    return Some(Err(err));
                };
                let err = DocumentError::Write {
                    path: Arc::unwrap_or_clone(job.path),
                    source: io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "save worker disconnected unexpectedly",
                    ),
                };
                self.last_background_issue = Some(background_issue_from_error(
                    BackgroundIssueKind::SaveFailed,
                    &err,
                ));
                return Some(Err(err));
            }
        };

        self.save_job = None;
        let close_after_job = self.close_after_job;
        self.close_after_job = false;
        let discard_save_result = self.discard_save_result;
        self.discard_save_result = false;
        Some(match state {
            Ok(completion) => {
                if discard_save_result {
                    let err = DocumentError::Write {
                        path: completion.path,
                        source: io::Error::other(
                            "background save result discarded after current session state changed",
                        ),
                    };
                    self.last_background_issue = Some(background_issue_from_error(
                        BackgroundIssueKind::SaveDiscarded,
                        &err,
                    ));
                    Err(err)
                } else {
                    match self
                        .doc
                        .finish_save(completion.path, completion.reload_after_save)
                    {
                        Ok(()) => {
                            self.last_background_issue = None;
                            if close_after_job {
                                self.finish_close();
                            } else {
                                self.generation = self.generation.wrapping_add(1);
                            }
                            Ok(())
                        }
                        Err(err) => {
                            self.last_background_issue = Some(background_issue_from_error(
                                BackgroundIssueKind::SaveFailed,
                                &err,
                            ));
                            Err(err)
                        }
                    }
                }
            }
            Err(err) => {
                self.last_background_issue = Some(background_issue_from_error(
                    BackgroundIssueKind::SaveFailed,
                    &err,
                ));
                Err(err)
            }
        })
    }

    pub(super) fn save_async(&mut self) -> Result<bool, SaveError> {
        if self.is_saving() {
            return Err(SaveError::InProgress);
        }
        if self.is_loading() {
            let path = self.load_report_path();
            return Err(SaveError::Io(DocumentError::Write {
                path,
                source: io::Error::other("cannot save while load is in progress"),
            }));
        }
        let Some(path) = self.current_path().map(|p| p.to_path_buf()) else {
            return Err(SaveError::NoPath);
        };
        self.save_to_async(path).map_err(SaveError::Io)
    }

    pub(super) fn save_as_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        self.save_to_async(path)
    }

    pub(super) fn save_to_async(&mut self, path: PathBuf) -> Result<bool, DocumentError> {
        if self.is_saving() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("save already in progress"),
            });
        }
        if self.is_loading() {
            return Err(DocumentError::Write {
                path,
                source: io::Error::other("cannot save while load is in progress"),
            });
        }

        if !self.doc.is_dirty() && self.current_path() == Some(path.as_path()) {
            return Ok(false);
        }

        let prepared = self.doc.prepare_save(&path);
        let total_bytes = prepared.total_bytes();
        let written_bytes = Arc::new(AtomicU64::new(0));
        let rx = spawn_save_worker(prepared, Arc::clone(&written_bytes));
        let job_path = Arc::new(path);

        self.save_job = Some(SaveJob {
            path: job_path,
            total_bytes,
            written_bytes,
            rx,
        });
        self.last_background_issue = None;
        Ok(true)
    }

    pub(super) fn background_activity(&self) -> BackgroundActivity {
        if let Some(progress) = self.loading_state() {
            BackgroundActivity::Loading(progress)
        } else if let Some(progress) = self.save_state() {
            BackgroundActivity::Saving(progress)
        } else {
            BackgroundActivity::Idle
        }
    }

    pub(super) fn read_viewport(&self, request: ViewportRequest) -> Viewport {
        self.doc.read_viewport(request)
    }

    pub(super) fn status(&self) -> DocumentSessionStatus {
        DocumentSessionStatus::new(
            self.generation(),
            self.doc.status(),
            self.background_activity(),
            self.background_issue().cloned(),
            self.close_pending(),
        )
    }

    fn finish_open(&mut self, doc: Document) {
        self.clear_dirty_after_open = !doc.is_dirty();
        self.doc = doc;
        self.generation = self.generation.wrapping_add(1);
    }
}

fn spawn_save_worker(
    prepared: crate::document::PreparedSave,
    written_bytes: Arc<AtomicU64>,
) -> mpsc::Receiver<Result<SaveCompletion, DocumentError>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = prepared.execute(written_bytes);
        let _ = tx.send(result);
    });
    rx
}

fn map_open_phase(phase: OpenProgressPhase) -> LoadPhase {
    match phase {
        OpenProgressPhase::OpeningStorage => LoadPhase::Opening,
        OpenProgressPhase::InspectingSource => LoadPhase::InspectingSource,
        OpenProgressPhase::PreparingIndex => LoadPhase::PreparingIndex,
        OpenProgressPhase::RecoveringSession => LoadPhase::RecoveringSession,
        OpenProgressPhase::Ready => LoadPhase::Ready,
    }
}

fn spawn_load_worker(
    path: PathBuf,
    total_bytes: u64,
    loaded_bytes: Arc<AtomicU64>,
    phase: Arc<AtomicU8>,
) -> mpsc::Receiver<Result<Document, DocumentError>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        phase.store(LoadPhase::Opening.as_raw(), Ordering::Relaxed);
        let result = Document::open_with_reporting(
            path,
            |completed_bytes| {
                loaded_bytes.store(completed_bytes.min(total_bytes), Ordering::Relaxed);
            },
            &mut |open_phase| {
                phase.store(map_open_phase(open_phase).as_raw(), Ordering::Relaxed);
            },
        );
        let _ = tx.send(result);
    });
    rx
}

fn background_issue_from_error(kind: BackgroundIssueKind, err: &DocumentError) -> BackgroundIssue {
    let (path, message) = match err {
        DocumentError::Open { path, source }
        | DocumentError::Map { path, source }
        | DocumentError::Write { path, source } => (path.clone(), source.to_string()),
        DocumentError::EditUnsupported { path, reason } => {
            (path.clone().unwrap_or_default(), (*reason).to_string())
        }
    };
    BackgroundIssue::new(kind, path, message)
}

fn missing_load_job_error() -> DocumentError {
    DocumentError::Open {
        path: PathBuf::new(),
        source: io::Error::other("background load job disappeared unexpectedly"),
    }
}

fn missing_save_job_error() -> DocumentError {
    DocumentError::Write {
        path: PathBuf::new(),
        source: io::Error::other("background save job disappeared unexpectedly"),
    }
}

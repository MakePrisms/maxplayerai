use std::error::Error;
use std::fmt::{self, Display};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event::{CURRENT_ENVELOPE_VERSION, Envelope, Event};

#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    pub envelopes: Vec<Envelope>,
    pub error: Option<ReadError>,
}

#[derive(Debug)]
pub enum LogError {
    Io(io::Error),
    Encode(serde_json::Error),
    ClockBeforeUnixEpoch,
}

impl Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Encode(error) => write!(f, "failed to encode event envelope: {error}"),
            Self::ClockBeforeUnixEpoch => write!(f, "system clock is before the Unix epoch"),
        }
    }
}

impl Error for LogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::ClockBeforeUnixEpoch => None,
        }
    }
}

impl From<io::Error> for LogError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for LogError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encode(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    Io {
        message: String,
    },
    Decode {
        line: usize,
        message: String,
    },
    UnsupportedVersion {
        line: usize,
        found: u16,
        current: u16,
    },
    SequenceGap {
        line: usize,
        expected: u64,
        found: u64,
    },
}

impl Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message } => write!(f, "I/O error while reading event log: {message}"),
            Self::Decode { line, message } => {
                write!(
                    f,
                    "failed to decode event envelope on line {line}: {message}"
                )
            }
            Self::UnsupportedVersion {
                line,
                found,
                current,
            } => write!(
                f,
                "unsupported event envelope version {found} on line {line}; current version is {current}"
            ),
            Self::SequenceGap {
                line,
                expected,
                found,
            } => write!(
                f,
                "event sequence gap on line {line}: expected {expected}, found {found}"
            ),
        }
    }
}

impl Error for ReadError {}

impl EventLog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let replay = read_log(&path, 0);
        let next_seq = replay
            .envelopes
            .last()
            .map_or(0, |envelope| envelope.seq.saturating_add(1));

        let file = OpenOptions::new().create(true).append(true).open(&path)?;

        Ok(Self {
            path,
            file,
            next_seq,
        })
    }

    pub fn append(&mut self, payload: Event) -> Result<Envelope, LogError> {
        let envelope = Envelope {
            v: CURRENT_ENVELOPE_VERSION,
            seq: self.next_seq,
            ts: unix_millis()?,
            payload,
        };

        serde_json::to_writer(&mut self.file, &envelope)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;

        self.next_seq += 1;
        Ok(envelope)
    }

    pub fn replay(&self, from_seq: u64) -> Replay {
        read_log(&self.path, from_seq)
    }
}

fn read_log(path: &Path, from_seq: u64) -> Replay {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Replay {
                envelopes: Vec::new(),
                error: None,
            };
        }
        Err(error) => {
            return Replay {
                envelopes: Vec::new(),
                error: Some(ReadError::Io {
                    message: error.to_string(),
                }),
            };
        }
    };

    let mut envelopes = Vec::new();
    let mut expected_seq = 0;

    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line_number = index + 1;
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                return Replay {
                    envelopes,
                    error: Some(ReadError::Io {
                        message: error.to_string(),
                    }),
                };
            }
        };

        if line.is_empty() {
            continue;
        }

        let envelope = match serde_json::from_str::<Envelope>(&line) {
            Ok(envelope) => envelope,
            Err(error) => {
                return Replay {
                    envelopes,
                    error: Some(ReadError::Decode {
                        line: line_number,
                        message: error.to_string(),
                    }),
                };
            }
        };

        if envelope.v > CURRENT_ENVELOPE_VERSION {
            return Replay {
                envelopes,
                error: Some(ReadError::UnsupportedVersion {
                    line: line_number,
                    found: envelope.v,
                    current: CURRENT_ENVELOPE_VERSION,
                }),
            };
        }

        if envelope.seq != expected_seq {
            return Replay {
                envelopes,
                error: Some(ReadError::SequenceGap {
                    line: line_number,
                    expected: expected_seq,
                    found: envelope.seq,
                }),
            };
        }

        expected_seq += 1;
        if envelope.seq >= from_seq {
            envelopes.push(envelope);
        }
    }

    Replay {
        envelopes,
        error: None,
    }
}

fn unix_millis() -> Result<u64, LogError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LogError::ClockBeforeUnixEpoch)?;
    Ok(duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::event::{
        ArtifactId, Event, GatewayStatus, HarnessCandidateId, JobExecutionStatus, JobId, ReceiptId,
    };

    use super::{EventLog, ReadError};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn round_trip_mixed_events_replays_in_sequence() {
        let path = test_log_path("round-trip");
        let mut log = EventLog::open(&path).expect("open log");
        let events = vec![
            Event::HarnessCandidateFound {
                candidate_id: HarnessCandidateId("codex".into()),
            },
            Event::GatewayStatusChanged {
                status: GatewayStatus::Online,
            },
            Event::JobExecutionChanged {
                job_id: JobId("job-1".into()),
                status: JobExecutionStatus::Queued,
            },
            Event::ArtifactProduced {
                artifact_id: ArtifactId("artifact-1".into()),
            },
        ];

        let appended = append_all(&mut log, events);
        let replay = log.replay(0);

        assert_eq!(replay.error, None);
        assert_eq!(replay.envelopes, appended);
        assert_eq!(
            replay
                .envelopes
                .iter()
                .map(|envelope| envelope.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn durable_after_reopen() {
        let path = test_log_path("durable");
        let appended = {
            let mut log = EventLog::open(&path).expect("open log");
            append_all(
                &mut log,
                vec![
                    Event::JobOffered {
                        job_id: JobId("job-1".into()),
                    },
                    Event::ReceiptSigned {
                        receipt_id: ReceiptId("receipt-1".into()),
                    },
                ],
            )
        };

        let reopened = EventLog::open(&path).expect("reopen log");
        let replay = reopened.replay(0);

        assert_eq!(replay.error, None);
        assert_eq!(replay.envelopes, appended);
    }

    #[test]
    fn late_attacher_replays_then_resumes_without_gap_or_duplicate() {
        let path = test_log_path("late-attacher");
        let mut writer = EventLog::open(&path).expect("open writer");
        let first_batch = append_all(
            &mut writer,
            (0..5)
                .map(|index| Event::JobOffered {
                    job_id: JobId(format!("job-{index}")),
                })
                .collect(),
        );

        let reader = EventLog::open(&path).expect("open reader");
        let first_replay = reader.replay(0);
        assert_eq!(first_replay.error, None);
        assert_eq!(first_replay.envelopes, first_batch);

        let second_batch = append_all(
            &mut writer,
            vec![
                Event::JobExecutionChanged {
                    job_id: JobId("job-5".into()),
                    status: JobExecutionStatus::Running,
                },
                Event::JobExecutionChanged {
                    job_id: JobId("job-5".into()),
                    status: JobExecutionStatus::Completed,
                },
            ],
        );

        let resume_from = first_replay.envelopes.last().expect("last envelope").seq + 1;
        let resumed = reader.replay(resume_from);

        assert_eq!(resumed.error, None);
        assert_eq!(resumed.envelopes, second_batch);
        assert_eq!(
            resumed
                .envelopes
                .iter()
                .map(|envelope| envelope.seq)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
    }

    #[test]
    fn truncated_final_line_returns_prefix_and_typed_error() {
        let path = test_log_path("truncated");
        let prefix = {
            let mut log = EventLog::open(&path).expect("open log");
            append_all(
                &mut log,
                vec![Event::JobOffered {
                    job_id: JobId("job-1".into()),
                }],
            )
        };
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open raw log")
            .write_all(br#"{"v":1,"seq":1,"ts":1,"payload":"#)
            .expect("write truncated line");

        let log = EventLog::open(&path).expect("reopen log with truncation");
        let replay = log.replay(0);

        assert_eq!(replay.envelopes, prefix);
        assert!(matches!(replay.error, Some(ReadError::Decode { .. })));
    }

    #[test]
    fn future_envelope_version_returns_typed_error() {
        let path = test_log_path("future-version");
        fs::write(
            &path,
            r#"{"v":2,"seq":0,"ts":1,"payload":{"type":"gateway.status_changed","data":{"status":"online"}}}"#,
        )
        .expect("write future version");

        let log = EventLog::open(&path).expect("open log");
        let replay = log.replay(0);

        assert!(replay.envelopes.is_empty());
        assert_eq!(
            replay.error,
            Some(ReadError::UnsupportedVersion {
                line: 1,
                found: 2,
                current: 1,
            })
        );
    }

    fn append_all(log: &mut EventLog, events: Vec<Event>) -> Vec<crate::Envelope> {
        events
            .into_iter()
            .map(|event| log.append(event).expect("append event"))
            .collect()
    }

    fn test_log_path(name: &str) -> PathBuf {
        let test_id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mobee-core-{name}-{}-{test_id}.jsonl",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    use std::io::Write;
}

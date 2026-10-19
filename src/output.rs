use std::{collections::BTreeMap, io::Write, process::ExitCode};

use serde::Serialize;

use crate::error::{AppError, Result};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Human,
    Json,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Data {
    Contexts {
        contexts: Vec<String>,
    },
    Namespaces {
        context: Option<String>,
        current_namespace: String,
        namespaces: Vec<String>,
    },
    Environment {
        context: Option<String>,
        namespace: String,
        kubeconfig: String,
        env: BTreeMap<String, String>,
        cleanup: Vec<String>,
    },
    Completed,
    Help {
        text: String,
    },
    Version {
        version: String,
    },
}

impl Mode {
    pub fn is_json(self) -> bool {
        self == Self::Json
    }

    pub fn progress(self, message: impl std::fmt::Display) {
        if !self.is_json() {
            let _ = writeln!(std::io::stderr().lock(), "{message}");
        }
    }

    pub fn child_output(self, bytes: &[u8]) {
        if !self.is_json() {
            let _ = std::io::stderr().lock().write_all(bytes);
        }
    }

    pub fn finish(self, result: Result<Data>) -> ExitCode {
        let mut stdout = std::io::stdout().lock();
        let (code, written) = match result {
            Ok(data) => (0, self.success(&mut stdout, &data)),
            Err(error) => {
                let code = error.exit_code;
                let written = if self.is_json() {
                    write_json(
                        &mut stdout,
                        &serde_json::json!({
                            "schema_version": 1,
                            "ok": false,
                            "error": error.json(),
                        }),
                    )
                } else {
                    writeln!(std::io::stderr().lock(), "{:?}", miette::Report::new(error))
                };
                (code, written)
            }
        };
        match written {
            Ok(()) => ExitCode::from(code),
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "Unable to write CLI output: {error}"
                );
                ExitCode::FAILURE
            }
        }
    }

    fn success(self, writer: &mut impl Write, data: &Data) -> std::io::Result<()> {
        if self.is_json() {
            return write_json(
                writer,
                &serde_json::json!({"schema_version": 1, "ok": true, "data": data}),
            );
        }
        match data {
            Data::Contexts { contexts } => {
                for context in contexts {
                    writeln!(writer, "{context}")?;
                }
            }
            Data::Help { text } => write!(writer, "{text}")?,
            Data::Version { version } => writeln!(writer, "tack {version}")?,
            _ => {}
        }
        Ok(())
    }
}

fn write_json(writer: &mut impl Write, data: &serde_json::Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(data).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)
}

pub fn invalid_arguments(error: clap::Error) -> AppError {
    AppError::new("invalid_arguments", "Invalid command-line arguments")
        .help("Run `tack --help` for usage information.")
        .detail(error.to_string())
        .with_exit_code(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_success_is_one_object_with_a_versioned_schema() {
        let mut output = Vec::new();
        Mode::Json
            .success(
                &mut output,
                &Data::Contexts {
                    contexts: vec!["a".into(), "b".into()],
                },
            )
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains('\x1b'));
        let data: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(data["schema_version"], 1);
        assert_eq!(data["ok"], true);
        assert_eq!(data["data"]["kind"], "contexts");
        assert_eq!(data["data"]["contexts"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn human_list_output_remains_plain_text() {
        let mut output = Vec::new();
        Mode::Human
            .success(
                &mut output,
                &Data::Contexts {
                    contexts: vec!["a".into()],
                },
            )
            .unwrap();
        assert_eq!(output, b"a\n");
    }
}

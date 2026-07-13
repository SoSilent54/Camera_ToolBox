//! 前端进程共用的结构化日志初始化。

use std::{io, panic, path::PathBuf};

use directories::ProjectDirs;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

const LOG_FILE_PREFIX: &str = "camera-toolbox.jsonl";
const LOG_RETENTION_FILES: usize = 7;
const FILE_DEFAULT_FILTER: &str = "warn,camera_toolbox_core=debug,camera_toolbox_app=debug,camera_toolbox_adapters=debug,camera_toolbox_logging=debug,camera_toolbox_gui=debug,camera_toolbox_cli=debug,camera_toolbox_tui=debug";

/// 保持异步日志 writer 存活；丢弃时会刷新已排队的日志。
pub struct LoggingGuard {
    _file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// 初始化当前进程的 console 与 JSONL 日志层。
///
/// 文件日志不可用时会退化为 console；初始化失败不得阻止前端启动。
#[must_use]
pub fn init() -> LoggingGuard {
    install_panic_hook();

    let console_filter = default_filter(if cfg!(debug_assertions) {
        "debug"
    } else {
        "warn"
    });
    let console_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_writer(io::stderr)
        .with_filter(console_filter);

    let Some(log_dir) = logging_directory() else {
        let _ = tracing_subscriber::registry()
            .with(console_layer)
            .try_init();
        tracing::warn!(
            operation = "logging_init",
            "log directory unavailable; using stderr only"
        );
        return LoggingGuard { _file_guard: None };
    };
    let file_appender = match create_rolling_appender(&log_dir) {
        Ok(appender) => appender,
        Err(error) => {
            let _ = tracing_subscriber::registry()
                .with(console_layer)
                .try_init();
            tracing::warn!(operation = "logging_init", error = %error, "file logging unavailable; using stderr only");
            return LoggingGuard { _file_guard: None };
        }
    };
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_writer(file_writer)
        .with_filter(default_filter(FILE_DEFAULT_FILTER));

    if tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()
        .is_err()
    {
        return LoggingGuard {
            _file_guard: Some(file_guard),
        };
    }

    tracing::debug!(
        operation = "logging_init",
        log_directory = %log_dir.display(),
        "structured file logging initialized"
    );
    LoggingGuard {
        _file_guard: Some(file_guard),
    }
}

/// 返回由平台约定解析出的日志目录，调用方不应手工拼接平台路径。
#[must_use]
pub fn logging_directory() -> Option<PathBuf> {
    ProjectDirs::from("org", "camera-toolbox", "Camera Toolbox").map(|directories| {
        directories
            .state_dir()
            .unwrap_or_else(|| directories.data_local_dir())
            .join("logs")
    })
}

fn default_filter(default_directive: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive))
}

/// 由 appender 原生实现目录创建、daily rotation 和最多 7 个匹配文件的保留。
fn create_rolling_appender(
    log_dir: &std::path::Path,
) -> Result<tracing_appender::rolling::RollingFileAppender, tracing_appender::rolling::InitError> {
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .max_log_files(LOG_RETENTION_FILES)
        .build(log_dir)
}

fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        tracing::error!(operation = "panic", panic = %info, "unhandled panic");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn unique_temp_dir() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("camera-toolbox-logging-{suffix}"))
    }

    fn application_log_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .expect("read log directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(LOG_FILE_PREFIX)
            })
            .count()
    }

    #[test]
    fn rolling_appender_keeps_at_most_configured_matching_logs() {
        let directory = unique_temp_dir();
        fs::create_dir_all(&directory).expect("create temp directory");
        for day in 1..=8 {
            fs::write(
                directory.join(format!("{LOG_FILE_PREFIX}.2026-07-{day:02}")),
                "{}\n",
            )
            .expect("write previous application log");
        }
        fs::write(directory.join("unrelated.jsonl"), "{}\n").expect("write unrelated log");

        let appender = create_rolling_appender(&directory).expect("build rolling appender");

        assert!(application_log_count(&directory) <= LOG_RETENTION_FILES);
        assert!(directory.join("unrelated.jsonl").exists());
        drop(appender);
        fs::remove_dir_all(directory).expect("remove temp directory");
    }

    #[test]
    fn non_blocking_guard_flushes_structured_jsonl_fields() {
        let directory = unique_temp_dir();
        fs::create_dir_all(&directory).expect("create temp directory");
        let appender = create_rolling_appender(&directory).expect("build rolling appender");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_writer(writer),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                operation = "logging_test",
                generation = 7_u64,
                observed_max = 1024_u16,
                "structured test event"
            );
        });
        drop(guard);

        let json_lines = fs::read_dir(&directory)
            .expect("read log directory")
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_to_string(entry.path()).ok())
            .flat_map(|content| content.lines().map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let event = json_lines
            .iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse JSONL"))
            .find(|event| event["fields"]["operation"] == "logging_test")
            .expect("find structured event");
        assert_eq!(event["level"], "WARN");
        assert_eq!(event["fields"]["generation"], 7);
        assert_eq!(event["fields"]["observed_max"], 1024);
        assert!(event["timestamp"].is_string());
        assert!(event["target"].is_string());
        assert!(event["threadId"].is_string());
        fs::remove_dir_all(directory).expect("remove temp directory");
    }

    #[test]
    fn appender_construction_error_is_recoverable() {
        let directory = unique_temp_dir();
        fs::create_dir_all(&directory).expect("create temp directory");
        let not_a_directory = directory.join("regular-file");
        fs::write(&not_a_directory, "not a directory").expect("write regular file");

        assert!(create_rolling_appender(&not_a_directory).is_err());
        fs::remove_dir_all(directory).expect("remove temp directory");
    }
}

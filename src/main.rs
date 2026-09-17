use clap::Parser;
use tracing_subscriber::{
    filter::{LevelFilter, Targets},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

fn log_filter(value: Option<&str>) -> Targets {
    value
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| Targets::new().with_default(LevelFilter::WARN))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(log_filter(std::env::var("LOGISHELL_LOG").ok().as_deref()))
        .init();
    match logishell::cli::run(logishell::cli::Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "logishell: {}",
                logishell::terminal::clean(&format!("{error:#}"))
            );
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn log_levels_and_module_filters_keep_warning_fallback() {
        for value in [None, Some(""), Some(" , "), Some("warn,logishell=invalid")] {
            assert_eq!(log_filter(value).default_level(), Some(LevelFilter::WARN));
        }
        let info = log_filter(Some("info"));
        assert!(info.would_enable("other", &Level::INFO));
        assert!(!info.would_enable("other", &Level::DEBUG));
        assert!(
            log_filter(Some("logishell=debug")).would_enable("logishell::runtime", &Level::DEBUG)
        );
        let modules = log_filter(Some("warn, logishell::remap=debug,,"));
        assert!(modules.would_enable("logishell::remap::diversion", &Level::DEBUG));
        assert!(!modules.would_enable("other", &Level::INFO));
    }
}

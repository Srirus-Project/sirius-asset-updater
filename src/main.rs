use sirius_asset_updater::{CatalogClient, Config, Error};
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    use sirius_asset_updater::config_env::Document;
    let (path, document) = match args.first().map(String::as_str) {
        Some(kind @ ("serve" | "export" | "publish" | "plan-storage")) if args.len() == 2 => (
            Some(std::path::PathBuf::from(&args[1])),
            match kind {
                "serve" => Document::Service,
                "export" => Document::Export,
                _ => Document::Publish,
            },
        ),
        _ => (None, Document::Download),
    };
    // The download configuration (local or SIRIUS_ASSET_CONFIG_URI) is read exactly once, so
    // logging and the command decode the same snapshot.
    let download = if args.is_empty() || args == ["check"] || args == ["probe"] {
        match sirius_asset_updater::config_source::read_download_config().await {
            Ok(text) => Some(text),
            Err(error) => {
                eprintln!("{error}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let logging = match &download {
        Some(text) => sirius_asset_updater::application_log::Config::from_text(text, document),
        None => sirius_asset_updater::application_log::Config::from_file(path.as_deref(), document),
    };
    let _logging = match logging.and_then(|c| c.init()) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match run(download).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error_code = error.code(), "Sirius asset command failed");
            eprintln!("{error}");
            std::process::ExitCode::from(if matches!(error, Error::Cancelled) {
                130
            } else {
                1
            })
        }
    }
}
async fn run(download: Option<String>) -> Result<(), Error> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("sirius-asset-updater {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.len() == 2 && args[0] == "serve" {
        return sirius_asset_updater::service::run_file(std::path::Path::new(&args[1])).await;
    }
    if args.len() == 2 && matches!(args[0].as_str(), "publish" | "plan-storage") {
        let config: sirius_asset_updater::storage::Command =
            sirius_asset_updater::config_env::load(
                std::path::Path::new(&args[1]),
                sirius_asset_updater::config_env::Document::Publish,
            )?;
        if args[0] == "plan-storage" {
            println!(
                "{}",
                sonic_rs::to_string_pretty(&config.storage.plan(config.region)?)
                    .map_err(|_| Error::Verification)?
            );
            return Ok(());
        }
        let publication = config.storage.run(&config.input, config.region).await?;
        println!(
            "{}",
            sonic_rs::to_string_pretty(&publication).map_err(|_| Error::Verification)?
        );
        return Ok(());
    }
    if args.len() == 2 && args[0] == "export" {
        let config: sirius_asset_updater::export::ExportConfig =
            sirius_asset_updater::config_env::load(
                std::path::Path::new(&args[1]),
                sirius_asset_updater::config_env::Document::Export,
            )?;
        let summary = config.run().await?;
        println!(
            "{}",
            sonic_rs::to_string_pretty(&summary).map_err(|_| Error::Verification)?
        );
        return if summary.complete {
            Ok(())
        } else {
            Err(Error::Export(
                "export incomplete or empty; see summary.json and resources.jsonl".into(),
            ))
        };
    }
    if args == ["--help"] {
        println!("usage: sirius-asset-updater [serve CONFIG | check | probe | verify DIRECTORY | verify-export DIRECTORY REGION | inspect-catalog FILE | inspect-keys FILE | export CONFIG | publish CONFIG | plan-storage CONFIG]\ncheck: offline config/secrets validation\nprobe: refresh/read Game API snapshot without CDN requests\nno arguments: execute configured downloads");
        return Ok(());
    }
    if args.len() == 3 && args[0] == "verify-export" {
        let region = match args[2].as_str() {
            "jp" => sirius_asset_updater::region::Region::Jp,
            "tw" => sirius_asset_updater::region::Region::Tw,
            "en" => sirius_asset_updater::region::Region::En,
            "kr" => sirius_asset_updater::region::Region::Kr,
            "cn" => return Err(Error::ReservedRegion),
            _ => return Err(Error::Config),
        };
        let report =
            sirius_asset_updater::export_verify::verify(std::path::Path::new(&args[1]), region)
                .await?;
        println!(
            "{}",
            sonic_rs::to_string_pretty(&report).map_err(|_| Error::Verification)?
        );
        return Ok(());
    }
    if args.len() == 2 && args[0] == "verify" {
        let report = sirius_asset_updater::verify::verify(std::path::Path::new(&args[1])).await?;
        println!(
            "{}",
            sonic_rs::to_string_pretty(&report).map_err(|_| Error::Verification)?
        );
        return Ok(());
    }
    if args.len() == 2 && (args[0] == "inspect-catalog" || args[0] == "inspect-keys") {
        use std::io::Read;
        let mut input = Vec::new();
        std::fs::File::open(&args[1])
            .map_err(|_| Error::Io)?
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut input)
            .map_err(|_| Error::Io)?;
        let catalog = sirius_asset_updater::catalog::Catalog::parse(&input)?;
        let output = if args[0] == "inspect-keys" {
            sonic_rs::to_string_pretty(&catalog.keys)
        } else {
            sonic_rs::to_string_pretty(&catalog)
        };
        println!("{}", output.map_err(|_| Error::Catalog)?);
        return Ok(());
    }
    if !args.is_empty() && args != ["check"] && args != ["probe"] {
        eprintln!(
            "usage: sirius-asset-updater [serve CONFIG | check | probe | verify DIRECTORY | verify-export DIRECTORY REGION | inspect-catalog FILE | inspect-keys FILE | export CONFIG | publish CONFIG | plan-storage CONFIG]"
        );
        return Err(Error::Config);
    }
    let config: Config = sirius_asset_updater::config_env::from_str(
        download.as_deref().ok_or(Error::Config)?,
        sirius_asset_updater::config_env::Document::Download,
    )?;
    let check = config.check()?;
    if args == ["check"] || !check.ready {
        println!(
            "{}",
            sonic_rs::to_string_pretty(&check).map_err(|_| Error::Config)?
        );
        if !check.ready {
            return Err(Error::Preflight);
        }
        return Ok(());
    }
    let client = CatalogClient::new(config)?;
    tokio::select! {
        result = async {
            if args == ["probe"] {
                let snapshot=client.probe().await?;
                println!("{}",sonic_rs::to_string_pretty(&snapshot).map_err(|_|Error::Snapshot)?);
            } else { println!("{}",client.fetch().await?.display()); }
            Ok(())
        } => result,
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|_|Error::Io)?;
            Err(Error::Cancelled)
        }
    }
}

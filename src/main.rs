use sirius_asset_updater::{CatalogClient, Config, Error};
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let path = if args.len() == 2 && matches!(args[0].as_str(), "serve" | "export" | "publish") {
        Some(std::path::PathBuf::from(&args[1]))
    } else if args.is_empty() || args == ["check"] || args == ["probe"] {
        Some(std::path::PathBuf::from(
            std::env::var("SIRIUS_ASSET_CONFIG_PATH")
                .unwrap_or_else(|_| "sirius-asset-config.yaml".into()),
        ))
    } else {
        None
    };
    let _logging = match sirius_asset_updater::application_log::Config::from_file(path.as_deref())
        .and_then(|c| c.init())
    {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match run().await {
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
async fn run() -> Result<(), Error> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("sirius-asset-updater {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.len() == 2 && args[0] == "serve" {
        return sirius_asset_updater::service::run_file(std::path::Path::new(&args[1])).await;
    }
    if args.len() == 2 && args[0] == "publish" {
        let config: sirius_asset_updater::storage::Command =
            yaml_serde::from_str(&std::fs::read_to_string(&args[1]).map_err(|_| Error::Config)?)
                .map_err(|_| Error::Config)?;
        let publication = config.storage.run(&config.input, config.region).await?;
        println!(
            "{}",
            sonic_rs::to_string_pretty(&publication).map_err(|_| Error::Verification)?
        );
        return Ok(());
    }
    if args.len() == 2 && args[0] == "export" {
        let config: sirius_asset_updater::export::ExportConfig =
            yaml_serde::from_str(&std::fs::read_to_string(&args[1]).map_err(|_| Error::Config)?)
                .map_err(|_| Error::Config)?;
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
        println!("usage: sirius-asset-updater [serve CONFIG | check | probe | verify DIRECTORY | verify-export DIRECTORY REGION | inspect-catalog FILE | inspect-keys FILE | export CONFIG | publish CONFIG]\ncheck: offline config/secrets validation\nprobe: refresh/read Game API snapshot without CDN requests\nno arguments: execute configured downloads");
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
            "usage: sirius-asset-updater [serve CONFIG | check | probe | verify DIRECTORY | verify-export DIRECTORY REGION | inspect-catalog FILE | inspect-keys FILE | export CONFIG | publish CONFIG]"
        );
        return Err(Error::Config);
    }
    let path = std::env::var("SIRIUS_ASSET_CONFIG_PATH")
        .unwrap_or_else(|_| "sirius-asset-config.yaml".into());
    let config: Config =
        yaml_serde::from_str(&std::fs::read_to_string(path).map_err(|_| Error::Config)?)
            .map_err(|_| Error::Config)?;
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

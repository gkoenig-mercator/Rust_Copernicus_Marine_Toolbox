//! Marine Data Store client configuration (list of STAC catalogues).

use anyhow::{Context, Result};
use log::debug;

use crate::http::Http;

const CONFIG_URL_DIRECT: &str =
    "https://s3.waw3-1.cloudferro.com/mdl-metadata/clientsConfigV1.json";
const CONFIG_URL_CDN: &str = "https://stac.marine.copernicus.eu/clients-config-v1";
const CONFIG_URL_STAGING: &str = "https://stac-dta.marine.copernicus.eu/clients-config-v1";

#[derive(Debug, Clone)]
pub struct CatalogueConfig {
    pub stac_catalogue_url: String,
    pub dataset_product_mapping_url: String,
    pub root_metadata_url: String,
}

#[derive(Debug, Clone)]
pub struct MarineDataStoreConfig {
    pub catalogues: Vec<CatalogueConfig>,
}

pub async fn fetch_config(http: &Http, staging: bool) -> Result<MarineDataStoreConfig> {
    let direct_url = std::env::var("COPERNICUSMARINE_CONFIG_URL")
        .unwrap_or_else(|_| CONFIG_URL_DIRECT.to_string());
    let url = if staging {
        CONFIG_URL_STAGING.to_string()
    } else {
        direct_url
    };
    debug!("Getting configuration from {url}");
    let config = match http.get_json(&url).await {
        Ok(config) => config,
        Err(e) if !staging => {
            debug!("Failed to get configuration from {url}: {e}, trying the CDN");
            http.get_json(CONFIG_URL_CDN).await.context(
                "Could not fetch the Marine Data Store configuration. \
                 Please check your internet connection.",
            )?
        }
        Err(e) => return Err(e),
    };
    let catalogues = config["catalogues"]
        .as_array()
        .context("Invalid configuration: no catalogues")?
        .iter()
        .map(|catalogue| {
            let field = |name: &str| {
                catalogue[name]
                    .as_str()
                    .map(str::to_string)
                    .with_context(|| format!("Invalid configuration: missing {name}"))
            };
            Ok(CatalogueConfig {
                stac_catalogue_url: field("stac")?,
                dataset_product_mapping_url: field("idMapping")?,
                root_metadata_url: field("stacRoot")?.trim_end_matches('/').to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(MarineDataStoreConfig { catalogues })
}

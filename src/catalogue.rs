//! STAC catalogue parsing. The models mirror the Python toolbox ones
//! (`copernicusmarine/catalogue_parser/models.py`) so the `describe` JSON
//! output has the same structure.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use futures::future::join_all;
use log::debug;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::config::{CatalogueConfig, MarineDataStoreConfig};
use crate::http::Http;
use crate::utils::parse_datetime;

pub const PART_DEFAULT: &str = "default";
pub const VERSION_DEFAULT: &str = "default";
const PART_SEPARATOR: &str = "--ext--";

#[derive(Debug, Clone, Serialize)]
pub struct Coordinate {
    pub coordinate_id: String,
    pub coordinate_unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunking_length: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_reference_coordinate: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_geometric_factor: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub axis: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Variable {
    pub short_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Vec<f64>>,
    pub coordinates: Vec<Coordinate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Service {
    pub service_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_short_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_format: Option<String>,
    pub uri: String,
    pub variables: Vec<Variable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms_metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arco_sparse_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Part {
    pub name: String,
    pub services: Vec<Service>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arco_updating_start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arco_updated_date: Option<String>,
    pub url_metadata: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Version {
    pub label: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dataset {
    pub dataset_id: String,
    pub dataset_name: String,
    pub product_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digital_object_identifier: Option<String>,
    pub versions: Vec<Version>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Product {
    pub title: String,
    pub product_id: String,
    pub thumbnail_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digital_object_identifier: Option<String>,
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing_level: Option<String>,
    pub production_center: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    pub datasets: Vec<Dataset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_user_manual: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_information_document: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synthesis_quality_overview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Catalogue {
    pub products: Vec<Product>,
}

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

impl Dataset {
    pub fn get_version(&self, force_version: Option<&str>) -> Result<&Version> {
        match force_version {
            None => self
                .versions
                .first()
                .with_context(|| format!("No version found for dataset {}.", self.dataset_id)),
            Some(label) => self
                .versions
                .iter()
                .find(|v| v.label == label)
                .with_context(|| {
                    format!(
                        "The specified version '{label}' cannot be found for dataset {}.\n\
                     Available versions are: {:?}.",
                        self.dataset_id,
                        self.versions.iter().map(|v| &v.label).collect::<Vec<_>>()
                    )
                }),
        }
    }
}

impl Version {
    pub fn get_part(&self, force_part: Option<&str>) -> Result<&Part> {
        let found = match force_part {
            Some(name) => self.parts.iter().find(|p| p.name == name),
            None => self
                .parts
                .iter()
                .find(|p| p.name == PART_DEFAULT)
                .or_else(|| self.parts.first()),
        };
        found.with_context(|| {
            format!(
                "No part '{}' found for version {}. Available parts are: {:?}.",
                force_part.unwrap_or(PART_DEFAULT),
                self.label,
                self.parts.iter().map(|p| &p.name).collect::<Vec<_>>()
            )
        })
    }

    /// Same ordering as the Python toolbox: released parts first, then parts
    /// retired later, then "default"/"latest"/"bathy" and alphabetical order.
    fn sort_parts(&mut self) -> (Option<String>, Option<String>) {
        let now = chrono::Utc::now();
        let is_not_released = |p: &Part| {
            p.released_date
                .as_deref()
                .and_then(|d| parse_datetime(d).ok())
                .is_some_and(|d| d > now)
        };
        let retired_ts = |p: &Part| {
            p.retired_date
                .as_deref()
                .and_then(|d| parse_datetime(d).ok())
                .map(|d| d.timestamp() as f64)
        };
        let max_retired = self
            .parts
            .iter()
            .filter_map(retired_ts)
            .fold(None, |acc: Option<f64>, t| {
                Some(acc.map_or(t, |a| a.max(t)))
            })
            .map(|m| m + 1.0)
            .unwrap_or(0.0);
        self.parts.sort_by(|a, b| {
            let key = |p: &Part| {
                (
                    is_not_released(p),
                    max_retired - retired_ts(p).unwrap_or(max_retired),
                    p.name != PART_DEFAULT,
                    p.name != "latest",
                    p.name != "bathy",
                    p.name.clone(),
                )
            };
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.parts
            .first()
            .map(|p| (p.released_date.clone(), p.retired_date.clone()))
            .unwrap_or((None, None))
    }
}

impl Dataset {
    /// Same ordering as the Python toolbox: newest released version first.
    fn sort_versions(&mut self) {
        let now = chrono::Utc::now();
        let mut not_released = std::collections::HashSet::new();
        let mut retired_dates = std::collections::HashMap::new();
        for version in &mut self.versions {
            let (released, retired) = version.sort_parts();
            if released
                .as_deref()
                .and_then(|d| parse_datetime(d).ok())
                .is_some_and(|d| d > now)
            {
                not_released.insert(version.label.clone());
            }
            if let Some(retired) = retired {
                retired_dates.insert(version.label.clone(), retired);
            }
        }
        // Python: sorted(key=(-(not released), retired, -(default), label), reverse=True)
        self.versions.sort_by(|a, b| {
            let key = |v: &Version| {
                (
                    !not_released.contains(&v.label),
                    retired_dates
                        .get(&v.label)
                        .cloned()
                        .unwrap_or_else(|| "9999-12-31".to_string()),
                    v.label != VERSION_DEFAULT,
                    v.label.clone(),
                )
            };
            key(b).cmp(&key(a))
        });
    }
}

impl Catalogue {
    /// Keep only the latest version and its released parts (default for `describe`).
    pub fn filter_only_official_versions_and_parts(&mut self) {
        let now = chrono::Utc::now();
        for product in &mut self.products {
            for dataset in &mut product.datasets {
                dataset.versions.truncate(1);
                if let Some(latest) = dataset.versions.first_mut() {
                    latest.parts.retain(|p| {
                        !p.released_date
                            .as_deref()
                            .and_then(|d| parse_datetime(d).ok())
                            .is_some_and(|d| d > now)
                    });
                    if latest.parts.is_empty() {
                        dataset.versions.clear();
                    }
                }
            }
            product.datasets.retain(|d| !d.versions.is_empty());
        }
        self.products.retain(|p| !p.datasets.is_empty());
    }
}

// ---------------------------------------------------------------------------
// STAC parsing
// ---------------------------------------------------------------------------

/// Split "cmems_xxx_202406--ext--bathy" into ("cmems_xxx", "202406", "bathy").
pub fn parse_full_dataset_id(full_id: &str) -> (String, String, String) {
    static VERSION_REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = VERSION_REGEX
        .get_or_init(|| Regex::new(r"^(.*?)(?:_([12]\d{3}(?:0[1-9]|1[0-2])))?$").unwrap());
    let (name_with_version, part) = match full_id.split_once(PART_SEPARATOR) {
        Some((name, part)) => (name, part.to_string()),
        None => (full_id, PART_DEFAULT.to_string()),
    };
    let captures = regex.captures(name_with_version).unwrap();
    let name = captures
        .get(1)
        .map_or(name_with_version, |m| m.as_str())
        .to_string();
    let version = captures
        .get(2)
        .map_or(VERSION_DEFAULT.to_string(), |m| m.as_str().to_string());
    (name, version, part)
}

struct DatasetItem {
    item_id: String,
    parsed_id: String,
    parsed_version: String,
    parsed_part: String,
    url: String,
    json: Value,
}

fn as_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn service_names(asset_name: &str) -> Option<(&'static str, &'static str)> {
    Some(match asset_name {
        "timeChunked" => ("arco-geo-series", "geoseries"),
        "geoChunked" => ("arco-time-series", "timeseries"),
        "platformChunked" => ("arco-platform-series", "platformseries"),
        "native" => ("original-files", "files"),
        "wmts" => ("wmts", "wmts"),
        "omi" => ("omi-arco", "omi-arco"),
        "static" => ("static-arco", "static-arco"),
        _ => return None,
    })
}

fn parse_coordinate(
    variable_id: &str,
    dimension: &str,
    dimension_metadata: &Value,
    properties: &Value,
) -> Coordinate {
    let coords = &dimension_metadata["coords"];
    let mut minimum_value = None;
    let mut values = None;
    if dimension == "time" {
        if let (Some(start), false) = (
            properties["admp_valid_start_date"].as_str(),
            coords["min"].is_null(),
        ) {
            minimum_value = Some(if coords["min"].is_i64() || coords["min"].is_u64() {
                parse_datetime(start)
                    .map(|d| Value::from(d.timestamp_millis()))
                    .unwrap_or_else(|_| Value::from(start))
            } else {
                Value::from(start)
            });
        } else if let (Some(index), Some(all_values)) = (
            properties["admp_valid_start_index"].as_u64(),
            coords["values"].as_array(),
        ) {
            values = Some(all_values.iter().skip(index as usize).cloned().collect());
        }
    }
    let chunking_length = match &dimension_metadata["chunkLen"] {
        Value::Object(map) => map.get(variable_id).cloned(),
        Value::Null => None,
        other => Some(other.clone()),
    }
    .filter(|v| !v.is_null());
    let cube_dimension = &properties["cube:dimensions"][dimension];
    let axis = as_string(&cube_dimension["axis"])
        .or_else(|| (cube_dimension["type"].as_str() == Some("temporal")).then(|| "t".to_string()));
    let not_null = |v: &Value| (!v.is_null()).then(|| v.clone());
    let mut coordinate = Coordinate {
        coordinate_id: dimension.to_string(),
        coordinate_unit: as_string(&dimension_metadata["units"]).unwrap_or_default(),
        minimum_value: minimum_value.or_else(|| not_null(&coords["min"])),
        maximum_value: not_null(&coords["max"]),
        step: coords["step"].as_f64(),
        values: values.or_else(|| coords["values"].as_array().cloned()),
        chunking_length,
        chunk_type: as_string(&dimension_metadata["chunkType"]),
        chunk_reference_coordinate: not_null(&dimension_metadata["chunkRefCoord"]),
        chunk_geometric_factor: not_null(&dimension_metadata["chunkGeometricFactor"][variable_id]),
        axis,
    };
    if dimension == "elevation" {
        // The toolbox exposes depth (positive down) instead of elevation
        coordinate.coordinate_id = "depth".to_string();
        let negate =
            |v: &Option<Value>| v.as_ref().and_then(Value::as_f64).map(|x| Value::from(-x));
        let (min, max) = (
            negate(&coordinate.maximum_value),
            negate(&coordinate.minimum_value),
        );
        coordinate.minimum_value = min;
        coordinate.maximum_value = max;
        if let Some(values) = &mut coordinate.values {
            for v in values.iter_mut() {
                if let Some(x) = v.as_f64() {
                    *v = Value::from(-x);
                }
            }
        }
    }
    coordinate
}

fn parse_service(item: &Value, asset_name: &str, asset: &Value) -> Option<Service> {
    let (service_name, short_name) = service_names(asset_name)?;
    let uri = as_string(&asset["href"])?;
    if uri.ends_with('/') {
        return None;
    }
    let properties = &item["properties"];
    if properties["admp_in_preparation"].as_bool() == Some(true)
        && matches!(service_name, "arco-geo-series" | "arco-time-series")
    {
        return None;
    }
    let media_type = asset["type"].as_str().unwrap_or_default();
    let service_format = if media_type.contains("zarr") {
        Some("zarr".to_string())
    } else if media_type.contains("sqlite3") {
        Some("sqlite".to_string())
    } else {
        None
    };
    let bbox = item["bbox"]
        .as_array()
        .map(|b| b.iter().filter_map(Value::as_f64).collect::<Vec<_>>());
    let view_dims = asset["viewDims"].as_object();
    let variables = properties["cube:variables"]
        .as_object()
        .map(|variables| {
            variables
                .values()
                .filter_map(|cube_variable| {
                    let variable_id = cube_variable["id"].as_str()?;
                    let variable_dims: Vec<&str> = cube_variable["dimensions"]
                        .as_array()
                        .map(|d| d.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    let coordinates = view_dims
                        .map(|dims| {
                            dims.iter()
                                .filter(|(dim, _)| variable_dims.contains(&dim.as_str()))
                                .map(|(dim, meta)| {
                                    parse_coordinate(variable_id, dim, meta, properties)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(Variable {
                        short_name: variable_id.to_string(),
                        standard_name: as_string(&cube_variable["standardName"]),
                        units: Some(as_string(&cube_variable["unit"]).unwrap_or_default()),
                        bbox: bbox.clone(),
                        coordinates,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let platforms_metadata = (service_name == "arco-platform-series")
        .then(|| as_string(&item["assets"]["platforms"]["href"]))
        .flatten();
    Some(Service {
        service_name: service_name.to_string(),
        service_short_name: Some(short_name.to_string()),
        service_format,
        uri,
        variables,
        platforms_metadata,
        arco_sparse_type: as_string(&properties["admp_sparse_type"]),
    })
}

fn parse_part(item: &DatasetItem) -> Option<Part> {
    let properties = &item.json["properties"];
    let retired_date = as_string(&properties["admp_retired_date"]);
    if let Some(retired) = retired_date.as_deref().and_then(|d| parse_datetime(d).ok()) {
        if retired < chrono::Utc::now() {
            return None;
        }
    }
    let services: Vec<Service> = item.json["assets"]
        .as_object()?
        .iter()
        .filter_map(|(name, asset)| parse_service(&item.json, name, asset))
        .collect();
    if services.is_empty() {
        return None;
    }
    Some(Part {
        name: item.parsed_part.clone(),
        services,
        retired_date,
        released_date: as_string(&properties["admp_released_date"]),
        arco_updating_start_date: as_string(&properties["admp_updating_start_date"]),
        arco_updated_date: as_string(&properties["admp_updated_data"]),
        url_metadata: item.url.clone(),
    })
}

fn build_dataset(items: &[&DatasetItem], product_id: &str, doi: Option<String>) -> Option<Dataset> {
    let example = items
        .iter()
        .find(|i| i.parsed_part == PART_DEFAULT)
        .or_else(|| items.first())?;
    let dataset_name = if example.parsed_part == PART_DEFAULT {
        as_string(&example.json["properties"]["title"]).unwrap_or_else(|| example.parsed_id.clone())
    } else {
        example.parsed_id.clone()
    };
    let mut dataset = Dataset {
        dataset_id: example.parsed_id.clone(),
        dataset_name,
        product_id: product_id.to_string(),
        digital_object_identifier: doi,
        versions: Vec::new(),
    };
    for item in items {
        let Some(part) = parse_part(item) else {
            debug!("Skipping part {} of {}", item.parsed_part, item.parsed_id);
            continue;
        };
        match dataset
            .versions
            .iter_mut()
            .find(|v| v.label == item.parsed_version)
        {
            Some(version) => version.parts.push(part),
            None => dataset.versions.push(Version {
                label: item.parsed_version.clone(),
                parts: vec![part],
            }),
        }
    }
    if dataset.versions.is_empty() {
        return None;
    }
    dataset.sort_versions();
    Some(dataset)
}

fn is_valid_stac_item(json: &Value) -> bool {
    let properties = &json["properties"];
    !properties["datetime"].is_null()
        || (!properties["start_datetime"].is_null() && !properties["end_datetime"].is_null())
}

fn link_href_by_title(collection: &Value, title: &str) -> Option<String> {
    collection["links"]
        .as_array()?
        .iter()
        .find(|l| l["title"].as_str() == Some(title))
        .and_then(|l| as_string(&l["href"]))
}

/// Fetch a product (STAC collection) and its datasets (STAC items).
/// When `force_dataset_id` is given, only that dataset is fetched.
pub async fn fetch_product(
    http: &Http,
    catalogue: &CatalogueConfig,
    product_id: &str,
    force_dataset_id: Option<&str>,
) -> Result<Option<Product>> {
    let root = &catalogue.root_metadata_url;
    let product_url = format!("{root}/{product_id}/product.stac.json");
    let collection = http.get_json(&product_url).await?;
    if collection["extent"]["spatial"].is_null() || collection["extent"]["temporal"].is_null() {
        debug!("Invalid STAC collection {product_url}");
        return Ok(None);
    }
    let item_links: Vec<String> = collection["links"]
        .as_array()
        .map(|links| {
            links
                .iter()
                .filter(|l| l["rel"].as_str() == Some("item"))
                .filter_map(|l| as_string(&l["href"]))
                .filter(|href| force_dataset_id.is_none_or(|id| href.contains(id)))
                .collect()
        })
        .unwrap_or_default();

    let fetches = item_links.iter().map(|href| {
        let url = format!("{root}/{product_id}/{href}");
        async move {
            match http.get_json(&url).await {
                Ok(json) => Some((url, json)),
                Err(e) => {
                    debug!("Failed to fetch dataset item {url}: {e:#}");
                    None
                }
            }
        }
    });
    let mut items: Vec<DatasetItem> = join_all(fetches)
        .await
        .into_iter()
        .flatten()
        .filter(|(_, json)| is_valid_stac_item(json))
        .filter_map(|(url, json)| {
            let item_id = as_string(&json["id"])?;
            let (parsed_id, parsed_version, parsed_part) = parse_full_dataset_id(&item_id);
            Some(DatasetItem {
                item_id,
                parsed_id,
                parsed_version,
                parsed_part,
                url,
                json,
            })
        })
        .collect();
    items.sort_by(|a, b| a.item_id.cmp(&b.item_id));

    let doi = as_string(&collection["sci:doi"]);
    let mut grouped: BTreeMap<&str, Vec<&DatasetItem>> = BTreeMap::new();
    for item in &items {
        grouped
            .entry(item.parsed_id.as_str())
            .or_default()
            .push(item);
    }
    let datasets: Vec<Dataset> = grouped
        .into_iter()
        .filter(|(id, _)| force_dataset_id.is_none_or(|f| f == *id))
        .filter_map(|(_, group)| build_dataset(&group, product_id, doi.clone()))
        .collect();
    if datasets.is_empty() {
        return Ok(None);
    }

    let production_center = collection["providers"]
        .as_array()
        .and_then(|providers| {
            providers.iter().find(|p| {
                p["roles"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|r| r.as_str() == Some("producer")))
            })
        })
        .and_then(|p| as_string(&p["name"]))
        .unwrap_or_default();
    let thumbnail_url = as_string(&collection["assets"]["thumbnail"]["href"])
        .map(|href| {
            if href.starts_with("http") {
                href
            } else {
                format!("{root}/{product_id}/{href}")
            }
        })
        .unwrap_or_default();
    let product_id_value = as_string(&collection["id"]).unwrap_or_else(|| product_id.to_string());
    Ok(Some(Product {
        title: as_string(&collection["title"]).unwrap_or_else(|| product_id_value.clone()),
        product_id: product_id_value,
        thumbnail_url,
        description: as_string(&collection["description"]),
        digital_object_identifier: doi,
        sources: collection["properties"]["sources"]
            .as_array()
            .map(|s| s.iter().filter_map(as_string).collect())
            .unwrap_or_default(),
        processing_level: as_string(&collection["properties"]["processingLevel"]),
        production_center,
        keywords: collection["keywords"]
            .as_array()
            .map(|k| k.iter().filter_map(as_string).collect()),
        datasets,
        product_user_manual: link_href_by_title(&collection, "Product User Manual"),
        quality_information_document: link_href_by_title(
            &collection,
            "Quality Information Document",
        ),
        synthesis_quality_overview: link_href_by_title(&collection, "Synthesis Quality Overview"),
    }))
}

async fn product_ids_for_dataset(
    http: &Http,
    catalogue: &CatalogueConfig,
    dataset_id: &str,
) -> Result<Vec<String>> {
    let mapping = http
        .get_json(&catalogue.dataset_product_mapping_url)
        .await?;
    Ok(mapping[dataset_id]
        .as_str()
        .map(|ids| ids.split(',').map(str::to_string).collect())
        .unwrap_or_default())
}

/// Fetch the metadata of one dataset (all its versions and parts).
pub async fn get_dataset_metadata(
    http: &Http,
    config: &MarineDataStoreConfig,
    dataset_id: &str,
) -> Result<Dataset> {
    let mut errors = Vec::new();
    for catalogue in &config.catalogues {
        let result: Result<Option<Dataset>> = async {
            for product_id in product_ids_for_dataset(http, catalogue, dataset_id).await? {
                if let Some(product) =
                    fetch_product(http, catalogue, &product_id, Some(dataset_id)).await?
                {
                    if let Some(dataset) = product
                        .datasets
                        .into_iter()
                        .find(|d| d.dataset_id == dataset_id)
                    {
                        return Ok(Some(dataset));
                    }
                }
            }
            Ok(None)
        }
        .await;
        match result {
            Ok(Some(dataset)) => return Ok(dataset),
            Ok(None) => {}
            Err(e) => errors.push(e),
        }
    }
    if errors.len() == config.catalogues.len() && !errors.is_empty() {
        return Err(errors.remove(0));
    }
    bail!("{dataset_id} Please check that the dataset exists and the input datasetID is correct.")
}

/// Parse a full catalogue (or a filtered part of it).
pub async fn parse_catalogue(
    http: &Http,
    config: &MarineDataStoreConfig,
    force_product_id: Option<&str>,
    force_dataset_id: Option<&str>,
    show_progress: bool,
) -> Result<Catalogue> {
    let mut products: Vec<Product> = Vec::new();
    for catalogue in &config.catalogues {
        let product_ids: Vec<String> = if let Some(dataset_id) = force_dataset_id {
            let ids = product_ids_for_dataset(http, catalogue, dataset_id).await?;
            if ids.is_empty() {
                bail!("{dataset_id} Please check that the dataset exists and the input datasetID is correct.");
            }
            match force_product_id {
                Some(p) if !ids.iter().any(|i| i == p) => bail!(
                    "{dataset_id} not part of {p} Please check that the dataset is part of the \
                     product and the input datasetID is correct."
                ),
                Some(p) => vec![p.to_string()],
                None => ids,
            }
        } else if let Some(product_id) = force_product_id {
            vec![product_id.to_string()]
        } else {
            let root = http.get_json(&catalogue.stac_catalogue_url).await?;
            root["links"]
                .as_array()
                .context("Invalid STAC catalogue")?
                .iter()
                .filter(|l| l["rel"].as_str() == Some("child"))
                .filter_map(|l| l["href"].as_str())
                .filter_map(|href| href.trim_start_matches("./").split('/').next())
                .map(str::to_string)
                .collect()
        };

        let progress = indicatif::ProgressBar::new(product_ids.len() as u64);
        if show_progress {
            progress.set_style(
                indicatif::ProgressStyle::with_template(
                    "Fetching products {bar:40} {pos}/{len} [{elapsed}]",
                )
                .unwrap(),
            );
        } else {
            progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        }
        let fetches = product_ids.iter().map(|product_id| {
            let progress = progress.clone();
            async move {
                let result = fetch_product(http, catalogue, product_id, force_dataset_id).await;
                progress.inc(1);
                match result {
                    Ok(product) => Ok(product),
                    Err(e) if force_product_id.is_some() => Err(e),
                    Err(e) => {
                        debug!("Failed to fetch product {product_id}: {e:#}");
                        Ok(None)
                    }
                }
            }
        });
        let fetched = join_all(fetches).await;
        progress.finish_and_clear();
        for product in fetched {
            let Some(product) = product? else { continue };
            if force_product_id.is_some_and(|p| p != product.product_id) {
                continue;
            }
            match products
                .iter_mut()
                .find(|p| p.product_id == product.product_id)
            {
                None => products.push(product),
                Some(existing) => {
                    for dataset in product.datasets {
                        if !existing
                            .datasets
                            .iter()
                            .any(|d| d.dataset_id == dataset.dataset_id)
                        {
                            existing.datasets.push(dataset);
                        }
                    }
                }
            }
        }
    }
    if let (Some(product_id), true) = (force_product_id, products.is_empty()) {
        bail!(
            "{product_id} Please check that the product exists and the input productID is correct."
        );
    }
    products.sort_by(|a, b| a.product_id.cmp(&b.product_id));
    Ok(Catalogue { products })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dataset_ids() {
        assert_eq!(
            parse_full_dataset_id("cmems_mod_glo_phy-cur_anfc_0.083deg_P1D-m_202406"),
            (
                "cmems_mod_glo_phy-cur_anfc_0.083deg_P1D-m".into(),
                "202406".into(),
                "default".into()
            )
        );
        assert_eq!(
            parse_full_dataset_id("cmems_mod_glo_phy_anfc_0.083deg_static_202211--ext--bathy"),
            (
                "cmems_mod_glo_phy_anfc_0.083deg_static".into(),
                "202211".into(),
                "bathy".into()
            )
        );
        assert_eq!(
            parse_full_dataset_id("some_dataset"),
            ("some_dataset".into(), "default".into(), "default".into())
        );
    }
}

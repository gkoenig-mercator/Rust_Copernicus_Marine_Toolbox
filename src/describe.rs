//! `describe` command: catalogue metadata as JSON.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::catalogue::parse_catalogue;
use crate::config::fetch_config;
use crate::http::Http;

pub struct DescribeOptions {
    pub product_id: Option<String>,
    pub dataset_id: Option<String>,
    pub contains: Vec<String>,
    pub show_all_versions: bool,
    pub return_fields: Option<String>,
    pub exclude_fields: Option<String>,
    pub max_concurrent_requests: usize,
    pub staging: bool,
    pub show_progress: bool,
}

pub async fn describe(options: &DescribeOptions) -> Result<Value> {
    let http = Http::new(None, options.max_concurrent_requests)?;
    let config = fetch_config(&http, options.staging).await?;
    let mut catalogue = parse_catalogue(
        &http,
        &config,
        options.product_id.as_deref(),
        options.dataset_id.as_deref(),
        options.show_progress,
    )
    .await?;
    if !options.show_all_versions {
        catalogue.filter_only_official_versions_and_parts();
    }
    let mut json = serde_json::to_value(&catalogue)?;
    if !options.contains.is_empty() {
        let search: Vec<String> = options.contains.iter().map(|s| s.to_lowercase()).collect();
        let products = json["products"].as_array().cloned().unwrap_or_default();
        json["products"] = Value::Array(
            products
                .iter()
                .filter_map(|product| search_and_filter(product, &search))
                .collect(),
        );
    }
    if let Some(fields) = &options.return_fields {
        let fields = split_fields(fields);
        if !fields.contains("all") {
            json = include_fields(&json, &fields).unwrap_or(Value::Object(Map::new()));
        }
    }
    if let Some(fields) = &options.exclude_fields {
        exclude_fields(&mut json, &split_fields(fields));
    }
    Ok(json)
}

fn split_fields(fields: &str) -> HashSet<String> {
    fields
        .split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

fn matches(value: &str, search: &[String]) -> bool {
    let value = value.to_lowercase();
    search.iter().any(|s| value.contains(s))
}

/// Port of the Python `search_and_filter`: an object is kept if any of its
/// fields matches; matching lists replace the original ones.
fn search_and_filter(value: &Value, search: &[String]) -> Option<Value> {
    let object = value.as_object()?;
    let mut updated = object.clone();
    let mut found = false;
    for (key, field) in object {
        match field {
            Value::String(s) if matches(s, search) => found = true,
            Value::Object(_) => {
                if let Some(filtered) = search_and_filter(field, search) {
                    updated.insert(key.clone(), filtered);
                    found = true;
                }
            }
            Value::Array(items) => {
                let filtered: Vec<Value> = items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Object(_) => search_and_filter(item, search),
                        Value::String(s) if matches(s, search) => Some(item.clone()),
                        _ => None,
                    })
                    .collect();
                if !filtered.is_empty() {
                    updated.insert(key.clone(), Value::Array(filtered));
                    found = true;
                }
            }
            _ => {}
        }
    }
    found.then_some(Value::Object(updated))
}

fn include_fields(value: &Value, fields: &HashSet<String>) -> Option<Value> {
    match value {
        Value::Object(object) => {
            let mut kept = Map::new();
            for (key, field) in object {
                if fields.contains(key) {
                    kept.insert(key.clone(), field.clone());
                } else if let Some(sub) = include_fields(field, fields) {
                    kept.insert(key.clone(), sub);
                }
            }
            (!kept.is_empty()).then_some(Value::Object(kept))
        }
        Value::Array(items) => {
            let kept: Vec<Value> = items
                .iter()
                .filter_map(|i| include_fields(i, fields))
                .collect();
            (!kept.is_empty()).then_some(Value::Array(kept))
        }
        _ => None,
    }
}

fn exclude_fields(value: &mut Value, fields: &HashSet<String>) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| !fields.contains(key));
            object.values_mut().for_each(|v| exclude_fields(v, fields));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| exclude_fields(v, fields)),
        _ => {}
    }
}

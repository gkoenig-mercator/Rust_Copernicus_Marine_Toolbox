//! `subset` command: extract an area/time/depth/variable subset from the
//! ARCO (Zarr) services and write it to NetCDF.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use serde::Serialize;
use serde_json::Value;

use crate::catalogue::{get_dataset_metadata, Service};
use crate::config::fetch_config;
use crate::credentials::get_and_check_user;
use crate::http::Http;
use crate::netcdf3::{AttrValue, Attribute, NcType, NetCdfFile, Variable};
use crate::utils::{parse_datetime, size_to_mb, unique_path, CfTimeUnits};
use crate::zarr::{ZarrArray, ZarrStore};

#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum CoordinatesSelectionMethod {
    /// Select points inside the requested interval
    Inside,
    /// Like inside, but fails if the interval is out of the dataset bounds
    StrictInside,
    /// Select the closest points to the interval bounds
    Nearest,
    /// Select the smallest interval containing the requested one
    Outside,
}

#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum VerticalAxis {
    Depth,
    Elevation,
}

#[derive(Debug, Clone)]
pub struct SubsetOptions {
    pub dataset_id: String,
    pub dataset_version: Option<String>,
    pub dataset_part: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub credentials_file: Option<PathBuf>,
    pub variables: Vec<String>,
    pub minimum_longitude: Option<f64>,
    pub maximum_longitude: Option<f64>,
    pub minimum_latitude: Option<f64>,
    pub maximum_latitude: Option<f64>,
    pub minimum_depth: Option<f64>,
    pub maximum_depth: Option<f64>,
    pub start_datetime: Option<String>,
    pub end_datetime: Option<String>,
    pub coordinates_selection_method: CoordinatesSelectionMethod,
    pub vertical_axis: VerticalAxis,
    pub service: Option<String>,
    pub output_directory: PathBuf,
    pub output_filename: Option<String>,
    pub overwrite: bool,
    pub skip_existing: bool,
    pub dry_run: bool,
    pub max_concurrent_requests: usize,
    pub show_progress: bool,
    pub staging: bool,
}

#[derive(Debug, Serialize)]
pub struct CoordinateExtent {
    pub minimum: Value,
    pub maximum: Value,
    pub unit: String,
    pub coordinate_id: String,
}

#[derive(Debug, Serialize)]
pub struct SubsetResponse {
    pub file_path: PathBuf,
    pub output_directory: PathBuf,
    pub filename: String,
    pub file_size: f64,
    pub data_transfer_size: f64,
    pub variables: Vec<String>,
    pub coordinates_extent: Vec<CoordinateExtent>,
    pub status: String,
    pub message: String,
    pub file_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Axis {
    X,
    Y,
    Z,
    T,
    Other,
}

fn axis_of(name: &str, array: Option<&ZarrArray>) -> Axis {
    let attr = |key: &str| {
        array
            .and_then(|a| a.attributes.get(key))
            .and_then(Value::as_str)
            .map(str::to_lowercase)
    };
    match name {
        "longitude" | "lon" => Axis::X,
        "latitude" | "lat" => Axis::Y,
        "elevation" | "depth" => Axis::Z,
        "time" => Axis::T,
        _ => match (attr("standard_name").as_deref(), attr("axis").as_deref()) {
            (Some("longitude"), _) => Axis::X,
            (Some("latitude"), _) => Axis::Y,
            (Some("time"), _) | (_, Some("t")) => Axis::T,
            _ => Axis::Other,
        },
    }
}

/// Indices of a 1-D monotonic coordinate selected by [min, max].
fn select_indices(
    values: &[f64],
    minimum: Option<f64>,
    maximum: Option<f64>,
    method: CoordinatesSelectionMethod,
    label: &str,
) -> Result<Vec<usize>> {
    let n = values.len();
    if n == 0 || (minimum.is_none() && maximum.is_none()) {
        return Ok((0..n).collect());
    }
    let first = values[0];
    let last = values[n - 1];
    let (low, high) = (first.min(last), first.max(last));
    let minimum = minimum.unwrap_or(low);
    let maximum = maximum.unwrap_or(high);
    if minimum > maximum {
        bail!("Minimum {label} ({minimum}) is greater than maximum {label} ({maximum})");
    }
    if method == CoordinatesSelectionMethod::StrictInside && (minimum < low || maximum > high) {
        bail!(
            "Some of your subset selection [{minimum}, {maximum}] for the {label} dimension \
             exceed the dataset coordinates [{low}, {high}]"
        );
    }
    if minimum > high || maximum < low {
        warn!(
            "Some of your subset selection [{minimum}, {maximum}] for the {label} dimension \
             exceed the dataset coordinates [{low}, {high}]"
        );
    }
    let nearest = |target: f64| -> usize {
        (0..n)
            .min_by(|&a, &b| {
                (values[a] - target)
                    .abs()
                    .partial_cmp(&(values[b] - target).abs())
                    .unwrap()
            })
            .unwrap()
    };
    let (minimum, maximum) = match method {
        CoordinatesSelectionMethod::Nearest => (values[nearest(minimum)], values[nearest(maximum)]),
        CoordinatesSelectionMethod::Outside => {
            let below = values
                .iter()
                .copied()
                .filter(|v| *v <= minimum)
                .fold(f64::NAN, f64::max);
            let above = values
                .iter()
                .copied()
                .filter(|v| *v >= maximum)
                .fold(f64::NAN, f64::min);
            (
                if below.is_nan() { low } else { below },
                if above.is_nan() { high } else { above },
            )
        }
        _ => (minimum, maximum),
    };
    let selected: Vec<usize> = (0..n)
        .filter(|&i| values[i] >= minimum && values[i] <= maximum)
        .collect();
    if selected.is_empty() {
        // Same behaviour as the Python toolbox: fall back on the nearest point
        return Ok(vec![nearest(minimum)]);
    }
    Ok(selected)
}

/// Equivalent longitude in [-180, 180[.
fn longitude_modulus(longitude: f64) -> f64 {
    let m = (longitude + 180.0).rem_euclid(360.0) - 180.0;
    if m == 0.0 {
        0.0
    } else {
        m
    }
}

/// numpy's remainder: result has the sign of the divisor.
fn numpy_remainder<T>(a: T, b: T) -> T
where
    T: Copy + PartialOrd + std::ops::Rem<Output = T> + std::ops::Add<Output = T> + Default,
{
    let zero = T::default();
    let r = a % b;
    if r != zero && ((r < zero) != (b < zero)) {
        r + b
    } else {
        r
    }
}

/// Longitude selection, port of the Python toolbox algorithm: bounds are
/// brought back to [-180, 180[ and, when the request crosses the
/// antimeridian, the longitudes are shifted to [start, start + 360[
/// (computed in the coordinate precision, like numpy).
/// Returns the selected indices and the output longitude values.
fn select_longitudes(
    values: &[f64],
    minimum: Option<f64>,
    maximum: Option<f64>,
    method: CoordinatesSelectionMethod,
    float32: bool,
) -> Result<(Vec<usize>, Vec<f64>)> {
    let identity = |indices: Vec<usize>| {
        let out = indices.iter().map(|&i| values[i]).collect();
        (indices, out)
    };
    let (Some(minimum), Some(maximum)) = (minimum, maximum) else {
        return Ok(identity(select_indices(
            values,
            minimum,
            maximum,
            method,
            "longitude",
        )?));
    };
    if minimum > maximum {
        bail!("Minimum longitude ({minimum}) is greater than maximum longitude ({maximum})");
    }
    let (min_mod, mut max_mod, shift_start) = if maximum - minimum >= 360.0 {
        (-180.0, 180.0, (maximum != 180.0).then_some(-180.0))
    } else {
        let (min_mod, max_mod) = (longitude_modulus(minimum), longitude_modulus(maximum));
        if max_mod < min_mod {
            (min_mod, max_mod, Some(min_mod))
        } else {
            (min_mod, max_mod, None)
        }
    };
    let Some(start) = shift_start else {
        return Ok(identity(select_indices(
            values,
            Some(min_mod),
            Some(max_mod),
            method,
            "longitude",
        )?));
    };
    if max_mod < min_mod {
        max_mod += 360.0;
    }
    // Start of the shifted window, from the dataset points for nearest/outside
    let start = match method {
        CoordinatesSelectionMethod::Nearest => values
            .iter()
            .copied()
            .min_by(|a, b| (a - start).abs().partial_cmp(&(b - start).abs()).unwrap())
            .unwrap_or(start),
        CoordinatesSelectionMethod::Outside => values
            .iter()
            .copied()
            .filter(|v| *v <= start)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
            .unwrap_or(start),
        _ => start,
    };
    // shifted = ((lon + c) % 360) - c, with c = 180 - (start + 180)
    let shifted: Vec<f64> = if float32 {
        let c = (180.0 - (start + 180.0)) as f32;
        values
            .iter()
            .map(|&v| (numpy_remainder(v as f32 + c, 360.0) - c) as f64)
            .collect()
    } else {
        let c = 180.0 - (start + 180.0);
        values
            .iter()
            .map(|&v| numpy_remainder(v + c, 360.0) - c)
            .collect()
    };
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| shifted[*a].partial_cmp(&shifted[*b]).unwrap());
    let sorted: Vec<f64> = order.iter().map(|&i| shifted[i]).collect();
    let selected = select_indices(&sorted, Some(min_mod), Some(max_mod), method, "longitude")?;
    Ok((
        selected.iter().map(|&k| order[k]).collect(),
        selected.iter().map(|&k| sorted[k]).collect(),
    ))
}

struct DimensionSelection {
    name: String,
    output_name: String,
    axis: Axis,
    indices: Vec<usize>,
    /// Output coordinate values (None if the dimension has no coordinate array)
    values: Option<Vec<f64>>,
}

fn choose_service<'a>(
    services: &'a [Service],
    requested: Option<&str>,
) -> Result<Vec<&'a Service>> {
    let zarr_services: Vec<&Service> = services
        .iter()
        .filter(|s| s.service_format.as_deref() == Some("zarr"))
        .filter(|s| {
            matches!(
                s.service_name.as_str(),
                "arco-geo-series" | "arco-time-series" | "static-arco" | "omi-arco"
            )
        })
        .collect();
    if let Some(requested) = requested {
        let found: Vec<&Service> = zarr_services
            .iter()
            .copied()
            .filter(|s| {
                s.service_name == requested || s.service_short_name.as_deref() == Some(requested)
            })
            .collect();
        if found.is_empty() {
            bail!(
                "Service '{requested}' not available for this dataset. Available services: {:?}",
                services.iter().map(|s| &s.service_name).collect::<Vec<_>>()
            );
        }
        return Ok(found);
    }
    let series: Vec<&Service> = zarr_services
        .iter()
        .copied()
        .filter(|s| {
            matches!(
                s.service_name.as_str(),
                "arco-geo-series" | "arco-time-series"
            )
        })
        .collect();
    if !series.is_empty() {
        return Ok(series);
    }
    if zarr_services.is_empty() {
        if services
            .iter()
            .any(|s| s.service_name == "arco-platform-series")
        {
            bail!("Sparse (in situ) datasets are not supported yet by this barebone version");
        }
        bail!("No ARCO (Zarr) service available for this dataset, try the 'get' command");
    }
    Ok(vec![zarr_services[0]])
}

fn format_lon(v: f64) -> String {
    format!("{:.2}{}", v.abs(), if v < 0.0 { "W" } else { "E" })
}

fn format_lat(v: f64) -> String {
    format!("{:.2}{}", v.abs(), if v < 0.0 { "S" } else { "N" })
}

fn range_text(min: f64, max: f64, format: impl Fn(f64) -> String) -> String {
    if min == max {
        format(min)
    } else {
        format!("{}-{}", format(min), format(max))
    }
}

fn min_max(values: &[f64]) -> (f64, f64) {
    values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        })
}

pub async fn subset(options: &SubsetOptions) -> Result<SubsetResponse> {
    let user = get_and_check_user(
        options.username.clone(),
        options.password.clone(),
        options.credentials_file.as_deref(),
    )
    .await?;
    let http = Http::new(Some(&user), options.max_concurrent_requests)?;
    let config = fetch_config(&http, options.staging).await?;
    let dataset = get_dataset_metadata(&http, &config, &options.dataset_id).await?;
    let version = dataset.get_version(options.dataset_version.as_deref())?;
    let part = version.get_part(options.dataset_part.as_deref())?;
    info!(
        "Selected dataset version: \"{}\", part: \"{}\"",
        version.label, part.name
    );
    let candidates = choose_service(&part.services, options.service.as_deref())?;

    // Open the first store to read the coordinates (identical in every service)
    let mut stores = Vec::new();
    for service in &candidates {
        stores.push((service, ZarrStore::open(&http, &service.uri).await?));
    }
    let (_, reference_store) = &stores[0];

    // Variables: by name, or by standard name
    let data_variable_names: Vec<String> = reference_store
        .arrays
        .values()
        .filter(|a| !(a.dimensions.len() == 1 && a.dimensions[0] == a.name))
        .filter(|a| !a.dimensions.is_empty())
        .map(|a| a.name.clone())
        .collect();
    let variables: Vec<String> = if options.variables.is_empty() {
        // Like the Python toolbox: the variables of the catalogue
        // (excludes auxiliary arrays such as lat_bnds/lon_bnds)
        let from_catalogue: Vec<String> = data_variable_names
            .iter()
            .filter(|name| {
                candidates[0]
                    .variables
                    .iter()
                    .any(|v| &v.short_name == *name)
            })
            .cloned()
            .collect();
        if from_catalogue.is_empty() {
            data_variable_names.clone()
        } else {
            from_catalogue
        }
    } else {
        options
            .variables
            .iter()
            .map(|requested| {
                if data_variable_names.contains(requested) {
                    return Ok(requested.clone());
                }
                data_variable_names
                    .iter()
                    .find(|name| {
                        reference_store.arrays[*name].attributes.get("standard_name").and_then(Value::as_str)
                            == Some(requested.as_str())
                    })
                    .cloned()
                    .with_context(|| {
                        format!(
                            "The variable '{requested}' is neither a variable or a standard name in \
                             the dataset. Available variables: {data_variable_names:?}"
                        )
                    })
            })
            .collect::<Result<_>>()?
    };
    if variables.is_empty() {
        bail!("No variable to download");
    }

    // Dimensions used by the selected variables, in order of appearance
    let mut dimension_names: Vec<String> = Vec::new();
    for variable in &variables {
        for dim in &reference_store.array(variable)?.dimensions {
            if !dimension_names.contains(dim) {
                dimension_names.push(dim.clone());
            }
        }
    }

    let start = options
        .start_datetime
        .as_deref()
        .map(parse_datetime)
        .transpose()?;
    let end = options
        .end_datetime
        .as_deref()
        .map(parse_datetime)
        .transpose()?;
    let mut time_units: Option<CfTimeUnits> = None;
    let mut selections: Vec<DimensionSelection> = Vec::new();
    for dim in &dimension_names {
        let coordinate_array = reference_store
            .arrays
            .get(dim)
            .filter(|a| a.shape.len() == 1);
        let axis = axis_of(dim, coordinate_array);
        let Some(coordinate_array) = coordinate_array else {
            let size = variables
                .iter()
                .filter_map(|v| {
                    let a = reference_store.arrays.get(v)?;
                    let position = a.dimensions.iter().position(|d| d == dim)?;
                    Some(a.shape[position])
                })
                .next()
                .unwrap_or(0);
            selections.push(DimensionSelection {
                name: dim.clone(),
                output_name: dim.clone(),
                axis,
                indices: (0..size).collect(),
                values: None,
            });
            continue;
        };
        let values = reference_store
            .read_coordinate(dim, options.max_concurrent_requests)
            .await?;
        let method = options.coordinates_selection_method;
        let (indices, output_values, output_name) = match axis {
            Axis::X if dim == "longitude" => {
                let (i, v) = select_longitudes(
                    &values,
                    options.minimum_longitude,
                    options.maximum_longitude,
                    method,
                    coordinate_array.dtype == crate::zarr::DType::parse("<f4")?,
                )?;
                (i, v, dim.clone())
            }
            Axis::Y if dim == "latitude" => {
                let i = select_indices(
                    &values,
                    options.minimum_latitude,
                    options.maximum_latitude,
                    method,
                    "latitude",
                )?;
                let v = i.iter().map(|&i| values[i]).collect();
                (i, v, dim.clone())
            }
            Axis::Z => {
                let is_elevation = dim == "elevation";
                let (min, max) = if is_elevation {
                    (
                        options.maximum_depth.map(|d| -d),
                        options.minimum_depth.map(|d| -d),
                    )
                } else {
                    (options.minimum_depth, options.maximum_depth)
                };
                let mut i = select_indices(&values, min, max, method, "depth")?;
                if is_elevation && options.vertical_axis == VerticalAxis::Depth {
                    // depth: positive down, increasing (reverse elevation order)
                    i.sort_by(|a, b| values[*b].partial_cmp(&values[*a]).unwrap());
                    let v = i.iter().map(|&i| -values[i] + 0.0).collect();
                    (i, v, "depth".to_string())
                } else {
                    let v = i.iter().map(|&i| values[i]).collect();
                    (i, v, dim.clone())
                }
            }
            Axis::T => {
                let units_text = coordinate_array
                    .attributes
                    .get("units")
                    .and_then(Value::as_str)
                    .context("Time coordinate without units")?;
                let units = CfTimeUnits::parse(units_text)?;
                let i = select_indices(
                    &values,
                    start.as_ref().map(|d| units.to_value(d)),
                    end.as_ref().map(|d| units.to_value(d)),
                    method,
                    "time",
                )?;
                time_units = Some(units);
                let v = i.iter().map(|&i| values[i]).collect();
                (i, v, dim.clone())
            }
            _ => {
                let all: Vec<usize> = (0..values.len()).collect();
                (all, values.clone(), dim.clone())
            }
        };
        debug!("Dimension {dim}: {} values selected", indices.len());
        selections.push(DimensionSelection {
            name: dim.clone(),
            output_name,
            axis,
            indices,
            values: Some(output_values),
        });
    }

    // Choose the service needing the fewest chunks
    let selection_for = |array: &ZarrArray| -> Vec<Vec<usize>> {
        array
            .dimensions
            .iter()
            .map(|d| {
                selections
                    .iter()
                    .find(|s| &s.name == d)
                    .unwrap()
                    .indices
                    .clone()
            })
            .collect()
    };
    let chunk_count = |store: &ZarrStore| -> Result<usize> {
        variables.iter().try_fold(0, |acc, v| {
            let array = store.array(v)?;
            Ok(acc + array.number_of_chunks(&selection_for(array)))
        })
    };
    let mut best = 0;
    let mut best_count = usize::MAX;
    for (i, (service, store)) in stores.iter().enumerate() {
        let count = chunk_count(store)?;
        debug!("Service {} needs {count} chunks", service.service_name);
        if count < best_count {
            best = i;
            best_count = count;
        }
    }
    let (service, store) = &stores[best];
    info!("Selected service: \"{}\"", service.service_name);

    // Sizes
    let mut output_bytes: u64 = 0;
    let mut transfer_bytes: u64 = 0;
    for v in &variables {
        let array = store.array(v)?;
        let selection = selection_for(array);
        let elements: usize = selection.iter().map(Vec::len).product();
        output_bytes += (elements * array.dtype.size) as u64;
        transfer_bytes += (array.number_of_chunks(&selection) * array.chunk_size_bytes()) as u64;
    }

    // Output file name
    let filename = match &options.output_filename {
        Some(name) if name.ends_with(".nc") => name.clone(),
        Some(name) => format!("{name}.nc"),
        None => {
            let mut parts = vec![options.dataset_id.clone()];
            let joined = variables.join("-");
            parts.push(if joined.len() > 15 && variables.len() > 1 {
                "multi-vars".to_string()
            } else {
                joined
            });
            // Same order as the Python toolbox: longitude, latitude, depth, time
            for axis in [Axis::X, Axis::Y, Axis::Z, Axis::T] {
                let Some(s) = selections.iter().find(|s| s.axis == axis) else {
                    continue;
                };
                let Some(values) = s.values.as_ref().filter(|v| !v.is_empty()) else {
                    continue;
                };
                let (lo, hi) = min_max(values);
                match s.axis {
                    Axis::X if s.name == "longitude" => parts.push(range_text(lo, hi, format_lon)),
                    Axis::Y if s.name == "latitude" => parts.push(range_text(lo, hi, format_lat)),
                    Axis::Z => parts.push(if lo == hi {
                        format!("{:.2}m", lo.abs())
                    } else {
                        format!("{:.2}-{:.2}m", lo.abs(), hi.abs())
                    }),
                    Axis::T => {
                        if let Some(units) = &time_units {
                            let format =
                                |v: f64| units.to_datetime(v).format("%Y-%m-%d").to_string();
                            let (a, b) = (format(lo), format(hi));
                            parts.push(if a == b { a } else { format!("{a}-{b}") });
                        }
                    }
                    _ => {}
                }
            }
            let mut name = parts.join("_");
            name.truncate(250);
            format!("{name}.nc")
        }
    };
    let mut file_path = options.output_directory.join(&filename);
    let mut file_status = "DOWNLOADED";
    if file_path.exists() {
        if options.skip_existing {
            file_status = "IGNORED";
        } else if options.overwrite {
            file_status = "OVERWRITTEN";
        } else {
            file_path = unique_path(&file_path);
        }
    }

    let coordinates_extent = [Axis::X, Axis::Y, Axis::T, Axis::Z]
        .iter()
        .filter_map(|axis| selections.iter().find(|s| s.axis == *axis))
        .filter_map(|s| {
            let values = s.values.as_ref().filter(|v| !v.is_empty())?;
            let (lo, hi) = min_max(values);
            let attrs = store.arrays.get(&s.name).map(|a| &a.attributes);
            let unit = attrs
                .and_then(|a| a.get("units"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (minimum, maximum, unit) = match (&time_units, s.axis) {
                (Some(u), Axis::T) => (
                    Value::from(u.to_datetime(lo).to_rfc3339()),
                    Value::from(u.to_datetime(hi).to_rfc3339()),
                    "iso8601".to_string(),
                ),
                _ => (Value::from(lo), Value::from(hi), unit),
            };
            Some(CoordinateExtent {
                minimum,
                maximum,
                unit,
                coordinate_id: s.output_name.clone(),
            })
        })
        .collect();

    let mut response = SubsetResponse {
        output_directory: options.output_directory.clone(),
        filename: file_path.file_name().unwrap().to_string_lossy().to_string(),
        file_path: file_path.clone(),
        file_size: size_to_mb(output_bytes),
        data_transfer_size: size_to_mb(transfer_bytes),
        variables: variables.clone(),
        coordinates_extent,
        status: "000".to_string(),
        message: "The request was successful.".to_string(),
        file_status: file_status.to_string(),
    };
    if options.dry_run {
        response.status = "001".to_string();
        response.message =
            "The request was run with the dry-run option. No data was downloaded.".to_string();
        return Ok(response);
    }
    if file_status == "IGNORED" {
        info!("File {} already exists, skipping.", file_path.display());
        return Ok(response);
    }
    info!(
        "Estimated size of the dataset file is {:.2} MB. Estimated size of the data that needs \
         to be transferred is {:.2} MB.",
        response.file_size, response.data_transfer_size
    );

    // Build the NetCDF file
    let mut file = NetCdfFile::default();
    for s in &selections {
        file.dimensions
            .push((s.output_name.clone(), s.indices.len()));
    }
    // CDF-5 is only needed when a single variable is larger than 4 GiB
    let mut largest_variable: u64 = 0;
    for v in &variables {
        let array = store.array(v)?;
        let elements: usize = selection_for(array).iter().map(Vec::len).product();
        largest_variable = largest_variable.max((elements * 8) as u64);
    }
    let cdf5 = largest_variable >= (u32::MAX as u64) - 4;
    file.cdf5 = cdf5;
    let dimension_index = |name: &str| selections.iter().position(|s| s.name == name).unwrap();

    // Coordinate variables
    for (d, s) in selections.iter().enumerate() {
        let (Some(values), Some(array)) = (&s.values, store.arrays.get(&s.name)) else {
            continue;
        };
        let dtype = if array.dtype.kind == crate::zarr::Kind::Float {
            array.dtype
        } else {
            crate::zarr::DType::parse("<f8")?
        };
        let data: Vec<u8> = values.iter().flat_map(|v| dtype.encode_f64(*v)).collect();
        let nc_type = NcType::for_dtype(&dtype, cdf5);
        let mut attributes = coordinate_attributes(&array.attributes, s.axis);
        if s.axis == Axis::Z {
            let positive = if s.output_name == "depth" {
                "down"
            } else {
                "up"
            };
            set_attribute(&mut attributes, "positive", positive);
            if s.output_name == "depth" {
                set_attribute(&mut attributes, "standard_name", "depth");
                set_attribute(&mut attributes, "long_name", "Depth");
            }
        }
        if s.axis == Axis::T {
            set_attribute(&mut attributes, "standard_name", "time");
            set_attribute(&mut attributes, "long_name", "Time");
            set_attribute(&mut attributes, "axis", "T");
            if let Some(units) = array.attributes.get("units").and_then(Value::as_str) {
                set_attribute(
                    &mut attributes,
                    "unit_long",
                    &python_title(&units.replace('_', " ")),
                );
            }
        }
        file.variables.push(Variable {
            name: s.output_name.clone(),
            dimensions: vec![d],
            attributes,
            nc_type,
            data,
            source_dtype: dtype,
        });
    }

    // Data variables
    let total_chunks: usize = variables
        .iter()
        .map(|v| {
            store
                .array(v)
                .map(|a| a.number_of_chunks(&selection_for(a)))
                .unwrap_or(0)
        })
        .sum();
    let progress = indicatif::ProgressBar::new(total_chunks as u64);
    if options.show_progress {
        progress.set_style(
            indicatif::ProgressStyle::with_template(
                "{bar:40} {pos}/{len} chunks [{elapsed}<{eta}, {per_sec}]",
            )
            .unwrap(),
        );
    } else {
        progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }
    for v in &variables {
        let array = store.array(v)?;
        let selection = selection_for(array);
        let data = store
            .read(
                v,
                &selection,
                options.max_concurrent_requests,
                Some(&progress),
            )
            .await?;
        let nc_type = NcType::for_dtype(&array.dtype, cdf5);
        let mut attributes = convert_attributes(&array.attributes, Some(nc_type));
        if let Some(fill) = array.fill_value {
            if !attributes.iter().any(|a| a.name == "_FillValue") {
                attributes.insert(
                    0,
                    Attribute {
                        name: "_FillValue".to_string(),
                        value: AttrValue::Numbers(nc_type, vec![fill]),
                    },
                );
            }
        }
        file.variables.push(Variable {
            name: v.clone(),
            dimensions: array
                .dimensions
                .iter()
                .map(|d| dimension_index(d))
                .collect(),
            attributes,
            nc_type,
            data,
            source_dtype: array.dtype,
        });
    }
    progress.finish_and_clear();

    file.attributes = convert_attributes(&store.attributes, None);
    std::fs::create_dir_all(&options.output_directory)?;
    let tmp_path = file_path.with_extension("nc.part");
    let write_path = tmp_path.clone();
    tokio::task::spawn_blocking(move || file.write(&write_path)).await??;
    std::fs::rename(&tmp_path, &file_path)?;
    info!("Successfully downloaded to {}", file_path.display());
    Ok(response)
}

/// Attributes kept on coordinates, as in the Python toolbox. The time
/// "units"/"calendar" are kept since they are its encoding.
fn coordinate_attributes(
    attributes: &serde_json::Map<String, Value>,
    axis: Axis,
) -> Vec<Attribute> {
    let mut kept = vec!["standard_name", "long_name", "units", "unit_long", "axis"];
    match axis {
        Axis::Z => kept.push("positive"),
        Axis::T => kept.push("calendar"),
        _ => {}
    }
    let filtered: serde_json::Map<String, Value> = attributes
        .iter()
        .filter(|(k, _)| kept.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    convert_attributes(&filtered, None)
}

/// Python's str.title(): capitalize the first letter of every word.
fn python_title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_is_letter = false;
    for c in text.chars() {
        if c.is_alphabetic() {
            if previous_is_letter {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            previous_is_letter = true;
        } else {
            out.push(c);
            previous_is_letter = false;
        }
    }
    out
}

fn set_attribute(attributes: &mut Vec<Attribute>, name: &str, value: &str) {
    let value = AttrValue::Text(value.to_string());
    match attributes.iter_mut().find(|a| a.name == name) {
        Some(a) => a.value = value,
        None => attributes.push(Attribute {
            name: name.to_string(),
            value,
        }),
    }
}

/// Convert Zarr attributes to NetCDF ones. For data variables, attributes
/// that must have the variable type (fill/valid values) are cast to it.
fn convert_attributes(
    attributes: &serde_json::Map<String, Value>,
    variable_type: Option<NcType>,
) -> Vec<Attribute> {
    attributes
        .iter()
        .filter_map(|(name, value)| {
            let mut value = AttrValue::from_json(value)?;
            if let Some(nc_type) = variable_type {
                if matches!(
                    name.as_str(),
                    "_FillValue" | "missing_value" | "valid_min" | "valid_max" | "valid_range"
                ) {
                    value = value.cast(nc_type);
                }
            }
            Some(Attribute {
                name: name.clone(),
                value,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_selection() {
        let values = [0.0, 1.0, 2.0, 3.0, 4.0];
        let m = CoordinatesSelectionMethod::Inside;
        assert_eq!(
            select_indices(&values, Some(0.5), Some(3.0), m, "x").unwrap(),
            vec![1, 2, 3]
        );
        // nothing inside -> nearest point
        assert_eq!(
            select_indices(&values, Some(1.4), Some(1.45), m, "x").unwrap(),
            vec![1]
        );
        let m = CoordinatesSelectionMethod::Outside;
        assert_eq!(
            select_indices(&values, Some(0.5), Some(2.5), m, "x").unwrap(),
            vec![0, 1, 2, 3]
        );
        let m = CoordinatesSelectionMethod::Nearest;
        assert_eq!(
            select_indices(&values, Some(0.6), Some(2.4), m, "x").unwrap(),
            vec![1, 2]
        );
    }

    #[test]
    fn antimeridian_selection() {
        let values: Vec<f64> = (0..360).map(|i| -180.0 + i as f64).collect();
        let (indices, out) = select_longitudes(
            &values,
            Some(178.0),
            Some(182.0),
            CoordinatesSelectionMethod::Inside,
            false,
        )
        .unwrap();
        assert_eq!(indices, vec![358, 359, 0, 1, 2]);
        assert_eq!(out, vec![178.0, 179.0, 180.0, 181.0, 182.0]);
        // not crossing: values are brought back to [-180, 180[ like Python
        let (indices, out) = select_longitudes(
            &values,
            Some(190.0),
            Some(192.0),
            CoordinatesSelectionMethod::Inside,
            false,
        )
        .unwrap();
        assert_eq!(indices, vec![10, 11, 12]);
        assert_eq!(out, vec![-170.0, -169.0, -168.0]);
    }
}

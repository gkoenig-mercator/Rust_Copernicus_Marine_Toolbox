# Rust_Copernicus_Marine_Toolbox

A barebone Rust version of the [Copernicus Marine Toolbox](https://github.com/mercator-ocean/copernicus-marine-toolbox)
with four commands: `login`, `describe`, `get` and `subset`.

## Build

```bash
cargo build --release
./target/release/copernicusmarine --help
```

## Usage

```bash
copernicusmarine login
copernicusmarine describe -i cmems_mod_glo_phy-cur_anfc_0.083deg_P1D-m
copernicusmarine get -i cmems_obs-sl_glo_phy-ssh_nrt_allsat-l4-duacs-0.25deg_P1D --filter "*_202407*"
copernicusmarine subset -i cmems_mod_glo_phy-cur_anfc_0.083deg_P1D-m -v uo \
  -x -10 -X -5 -y 40 -Y 45 -z 0 -Z 10 -t 2025-01-01 -T 2025-01-05
```

Options and credentials (file, environment variables, `.netrc`) are the same as in the Python toolbox.
`subset` writes NetCDF classic (CDF-2) files.

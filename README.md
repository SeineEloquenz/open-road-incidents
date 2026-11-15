# open-road-incidents

Collects open road incident and roadworks feeds from European road authorities, matches them to OpenStreetMap ways with Valhalla and publishes them as static files. Active closures are written into Valhalla's live traffic, so the same Valhalla routes around them.

## Sources

| ID | Country | Format | Licence |
|---|---|---|---|
| `fi-digitraffic` | Finland | DATEX II 3.7 plus GeoJSON for geometry | CC BY 4.0 |
| `fr-dir` | France, non-conceded national roads | DATEX II 2 | Licence Ouverte 2.0 |
| `lu-cita` | Luxembourg | DATEX II 3.6 | CC0 |
| `nl-ndw` | Netherlands | DATEX II 3 | CC0 |
| `be-vlaanderen` | Flanders | DATEX II 3, Lambert 72 | CC BY |
| `es-dgt` | Spain, without Basque Country and Catalonia | DATEX II 3.7 | unconfirmed |
| `de-autobahn` | German motorways | Autobahn GmbH JSON | unconfirmed |

The DGT and Autobahn GmbH have not published reuse terms. Their data is marked `unconfirmed` in every output until they confirm in writing.

## Running

```sh
nix develop
docker compose up -d          # Valhalla, Luxembourg by default
cargo run --release -- once   # fetch, match and publish once
cargo run --release -- run    # keep polling
cargo run --release -- check nl-ndw
```

Valhalla builds tiles from every `.osm.pbf` in `.valhalla/`. Add extracts for the covered countries and restart with `VALHALLA_FORCE_REBUILD=True`. Incidents outside the loaded extracts get `match_status: failed`.

The config is read from `config.toml` or `$ORI_CONFIG`. Set `user_agent` to something that identifies you before polling public feeds. With `traffic_extract` set, closures are written to that `traffic.tar`, which must be writable and created with `valhalla_build_extract -t`.

## Closures

Valhalla only applies closures to `auto` routes that set `"date_time": {"type": 0}`. Bicycle and pedestrian routes ignore them. Matched edge ids belong to one graph build, so a new graph needs Valhalla and this service restarted together.

## Server

`nixosModules.default` runs Valhalla and the collector as `services.open-road-incidents`. With `virtualHost` set, nginx serves `out/v1/` and passes only `/route` and `/status` to Valhalla, rate limited per client.

```nix
services.open-road-incidents = {
  enable = true;
  virtualHost = "roads.example.org";
};
```

The graph is built elsewhere and uploaded:

```sh
nix run .#build-graph -- graph                     # the covered countries, or pass regions like europe/luxembourg
rsync graph/work/{valhalla_tiles,traffic}.tar server:/var/lib/open-road-incidents/valhalla/incoming/
ssh server systemctl restart valhalla              # swaps in the new graph and restarts the collector
```

`CONCURRENCY` limits build threads, which lowers memory use.

## Output

Everything is written to `out/v1/`, ready for static hosting.

| Path | Content |
|---|---|
| `{cc}.geojson` | All current and planned incidents for a country |
| `avoid/{cc}.json` | Matched road closures as OSM way IDs, for routers |
| `tiles/{z}/{x}/{y}.pbf` | Vector tiles for zoom 6 to 12, layer `incidents` |
| `sources.json` | Licence, attribution and freshness per source |

Geometry always comes from the source, never from OSM. When a feed only gives start and end points the line between them is straight and `geometry_exact` is false. Clients must show the attribution of each source.

Records only matched through a route between their start and end point cover one direction of travel.

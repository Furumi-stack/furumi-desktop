# Architecture

Furumi Desktop is a modular monolith: it ships as one process and one binary,
while keeping reusable backend services independent from the desktop UI.

## Invariants

1. The UI renders state and emits intent; it never performs backend work.
2. Application state changes through deterministic reducers.
3. Backend state is authoritative for playback, queue, library and operations.
4. Navigation is frontend state: the backend does not know which panel is open.
5. Commands use a bounded channel; snapshots use a coalescing watch channel.
6. Long operations carry request IDs and cancellation tokens. Stale results are
   rejected before they reach authoritative state.
7. Local numeric IDs remain compatible with Furumi (`i64`). Stable track
   identity is an optional normalized `b3:<64 hex>` content ID.
8. Audio output runs on a dedicated worker thread. Device access, decoding and
   playback never block the UI or backend actor.
9. Durable settings belong to the backend. UI edits are projected immediately,
   then persisted by a dedicated worker without blocking the UI or actor loop.
10. Catalog entities use source-aware keys. Local and federated providers map
    into the same artist, release, track, and artwork contracts before a
    snapshot reaches the application or UI.
11. UI events identify catalog items by stable source-aware keys. A row index
    is never used as track identity because federation can reorder a list while
    results are arriving.

## Data flow

```text
Slint callback -> UiAction -> reducer -> BackendCommand -> backend actor
      ^                                              |
      |                  BackendSnapshot <-----------+
      +---- UI projection <- reducer/state update
```

The desktop application uses typed in-process channels. `backend-api` contains
no Slint, Tokio, database or transport types, so a server adapter can map the
same semantics onto another transport without making desktop pay for HTTP.

## Catalog providers and artwork

The local SQLite library and federation are catalog providers, not separate
sets of screens or view models. The backend merges provider results into
`LibrarySnapshot`; `CatalogSource`, `ArtistKey`, and `ReleaseKey` preserve
identity and provenance across that merge. Release tracks are normalized by
disc and metadata track number after every merge, with local records taking
precedence over equivalent remote records.

Artwork is asynchronously resolved. A provider may initially emit an entity
without a URI, fetch or cache its image, and publish a newer snapshot with the
same entity key and a local URI. Slint renders either that resolved image or
the common placeholder and never performs filesystem or network I/O.

## Crates

- `domain`: identifiers, entities and queue rules.
- `backend-api`: commands, snapshots, operation state and errors.
- `application`: frontend navigation state, reducers and UI projections.
- `backend`: actor/runtime orchestration, catalog federation, connected-device
  synchronization, persistence and the audio engine.
- `platform-desktop`: narrow native OS adapters such as the folder picker.
- `ui`: Slint components and the adapter connecting callbacks to state.
- `apps/desktop`: composition root only.

## Settings persistence

The backend stores settings in `furumi-desktop.sqlite3` under the platform
application-data directory selected by `directories::ProjectDirs`. Schema
changes are ordered migrations recorded in `schema_migrations`; each migration
runs in a transaction. The settings writer owns its SQLite connection on a
dedicated thread and coalesces bursts of edits before writing the latest full
snapshot. The configured device name is also written to the connected-device
identity and published through the device-profile operation log.

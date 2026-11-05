//! Implementation half of the `pets` sample: the request DTOs, the store helpers and the six
//! handler fns. The routes that mount them are declared in `pets-route.rs`, the sibling file;
//! nothing in here names a verb, a path or a summary.
//!
//! This file is never discovered by name - the scanner takes only `*-route.rs` files
//! (`build_support/matching.rs`) - so it compiles exactly because the route file pulls it in
//! with `#[path = "pets-core.rs"] mod handlers;` and it becomes `<route module>::handlers`,
//! which never shows up in the generated table on its own. Naming this file `pets-route.rs`
//! instead would be the mistake to avoid: the scanner would discover it as a route module of
//! its own, and the generated table asks every discovered module for `register` AND
//! `ENDPOINTS`.
//!
//! # What this example adds over `health`/`echo`
//!
//! `health` and `echo` prove the discovery wiring. This module - and its sibling
//! `examples/endpoints/locations/locations-core.rs`, kept deliberately parallel so the two can
//! be diffed - shows what one endpoint tree has to carry besides routes: request DTOs, the
//! status codes a REST API is expected to answer with, and persistence through
//! `AppState::database_root`, the only writable boundary a handler is given
//! (`src/server/mod.rs:65-69`). It is fully self-contained on purpose: a shared helper module
//! would hide what one endpoint has to carry by itself.
//!
//! # Persistence, and the simplifications that come with it
//!
//! * One JSON array file for the whole resource, `<database_root>/pets/pets.json`.
//!   Endpoints create their own folders below the root and nothing is created eagerly
//!   (`src/server/mod.rs:83-101`), so the folder and the file appear on the first write. A
//!   missing file is an empty store, never an error: `GET` on a fresh root answers `[]`.
//! * A write is `create_dir_all` + `fs::write` of the whole array - no temp file, no rename,
//!   no append log. That is the accepted example-grade simplification, and it is also what
//!   keeps the file a shape a person can read in an editor.
//! * Every read-modify-write takes `STORE_LOCK` and uses plain `std::fs` inside it. Blocking
//!   a tokio worker is fine at this scale, and the critical section never spans an `.await`,
//!   which is what keeps the handler futures `Send` as axum requires. The lock is
//!   process-local, so two servers sharing one `database_root` can still lose a write.
//! * A store file that exists but does not parse answers 500 and is left exactly as it is:
//!   nothing here overwrites or "heals" data it did not understand.
//! * The request body cap (an oversized POST/PUT/PATCH body is 413) belongs to the server's
//!   `DefaultBodyLimit` layer (`src/server/mod.rs:44-53`), not to this file.
//!
//! # Why the DTOs and `Failure` are `pub` while the module itself is private
//!
//! The route file's `crate::service_routes!` block emits `pub fn register(..)`
//! (`src/server/metadata.rs`), and that fn hands every handler below to `axum::routing::on`,
//! so each type in a handler signature is part of a public interface: a private one is a
//! hard rustc error (`E0446`, private type in a public interface) plus the
//! `private_interfaces` warning. `Pet`, `PetInput`, `PetPatch` and `Failure` are therefore
//! `pub`, and nothing else is - `ErrorBody`, the store constants and the store helpers stay
//! private because they never reach a signature.

use std::fs;
use std::io;
// `std::path::Path` is deliberately NOT imported: below this module the bare name `Path`
// belongs to `axum::extract::Path`, the extractor every `{id}` handler uses (and the spelling
// `examples/endpoints/echo/echo-core.rs` establishes). The two store helpers name the
// filesystem type in full instead of shadowing one of them.
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use axum::Json;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Deserialize;
use serde::Serialize;

/// Folder below `AppState::database_root` this resource owns. The root is shared between
/// endpoints, so a resource names itself exactly once and never writes above its folder.
const STORE_DIR: &str = "pets";

/// The single file behind the whole collection.
const STORE_FILE: &str = "pets.json";

/// The one message every storage failure answers with, so the 500 body cannot drift away
/// from `{"error":"pet storage failed"}`.
const STORE_ERROR: &str = "pet storage failed";

/// Serialises this resource's read-modify-write cycles (see the module doc for what that
/// does and does not protect against). `Mutex::new` is `const`, so no `OnceLock` wrapper is
/// needed to build the static.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// The stored record, and the body of every successful response of this resource:
/// `{ "id": 1, "name": "Ada", "species": "cat", "age": 3 }`.
///
/// Field order is declaration order is the wire order. `Deserialize` is derived because the
/// store file holds this same shape (including `id`); a POST/PUT body does not, which is
/// what [`PetInput`] is for. `Clone` exists so a handler can answer with the exact record it
/// just wrote.
#[derive(Clone, Deserialize, Serialize)]
pub struct Pet {
    id: u64,
    name: String,
    species: String,
    age: u8,
}

/// Body of `POST /v1/services/pets` and of `PUT /v1/services/pets/{id}`:
/// `{ "name": "Ada", "species": "cat", "age": 3 }`.
///
/// All three fields are required and there is no `id`: ids are assigned by the store, so a
/// body that omits a field or sends the wrong type is refused by serde before this module
/// runs (`Json` rejection: 422 for a shape mismatch, 400 for malformed JSON).
#[derive(Deserialize)]
pub struct PetInput {
    name: String,
    species: String,
    age: u8,
}

/// Body of `PATCH /v1/services/pets/{id}`: any subset of
/// `{ "name": ..., "species": ..., "age": ... }`.
///
/// Every field is `#[serde(default)] Option<..>`, which is what makes an absent key and a
/// literal `null` mean the same thing - "keep the stored value" - and `{}` is a valid no-op.
/// An `id` in the body is ignored: like `PUT`, `PATCH` cannot renumber a record.
#[derive(Deserialize)]
pub struct PetPatch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    species: Option<String>,
    #[serde(default)]
    age: Option<u8>,
}

/// The error wire shape, shared by both failures this resource can answer:
/// `{ "error": "no pet with id 7" }`.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// Everything that stops a handler from answering 2xx: an unknown id, or a store file it
/// could not read or rewrite.
pub enum Failure {
    /// No record carries this id; the payload is the id the message names.
    Missing(u64),

    /// `io`/`serde_json` failure on the store file, including a corrupt store file. The
    /// concrete error is dropped on purpose: the HTTP contract has exactly one message
    /// ([`STORE_ERROR`]) for every storage failure, and a filesystem path must not leak into
    /// a response (`src/server/mod.rs:59-64`).
    Store,
}

impl From<io::Error> for Failure {
    fn from(_: io::Error) -> Self {
        Self::Store
    }
}

impl From<serde_json::Error> for Failure {
    fn from(_: serde_json::Error) -> Self {
        Self::Store
    }
}

impl IntoResponse for Failure {
    /// 404 `{"error":"no pet with id <id>"}` or 500 `{"error":"pet storage failed"}`.
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Missing(id) => (StatusCode::NOT_FOUND, format!("no pet with id {id}")),
            Self::Store => (StatusCode::INTERNAL_SERVER_ERROR, STORE_ERROR.to_owned()),
        };
        (status, Json(ErrorBody { error })).into_response()
    }
}

/// `<database_root>/pets/pets.json`, derived only from the state's writable boundary.
fn store_path(state: &crate::server::AppState) -> PathBuf {
    state.database_root.join(STORE_DIR).join(STORE_FILE)
}

/// Takes the store lock for the rest of one handler body.
///
/// A poisoned lock is still usable: the only panics possible inside a section are bugs in
/// `fs`/`serde_json`, and no section leaves the file half-written (it writes it exactly
/// once, with one `fs::write` call).
fn lock_store() -> MutexGuard<'static, ()> {
    STORE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads the whole array.
///
/// A missing file (or a missing folder, which the OS also reports as "not found") is the
/// empty store. Anything else - an unreadable directory, a corrupt file - is
/// [`Failure::Store`], and this function never touches the file it could not parse.
fn load(path: &std::path::Path) -> Result<Vec<Pet>, Failure> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_slice(&bytes)?)
}

/// Rewrites the whole array, creating the folder on the way if this is the first write.
fn save(path: &std::path::Path, records: &[Pet]) -> Result<(), Failure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec(records)?)?;
    Ok(())
}

/// Position of `id` in the stored array, or `None`, which every caller turns into the 404.
fn index_of(records: &[Pet], id: u64) -> Option<usize> {
    records.iter().position(|record| record.id == id)
}

/// Ids come from the store only: `max(stored id) + 1`, so the first pet is 1.
///
/// Saturating on purpose: a hand-edited store holding `u64::MAX` must produce a duplicate id
/// (or a later 500), never a panic inside a request.
fn next_id(records: &[Pet]) -> u64 {
    let highest = records.iter().map(|record| record.id).max();
    highest.unwrap_or(0).saturating_add(1)
}

/// `GET /v1/services/pets` -> `[{ "id", "name", "species", "age" }, ...]`, 200.
///
/// The array is returned in stored order, which is insertion order: ids only ever grow and
/// new records are appended, so the two are the same sequence and the list is ascending by
/// id. An empty store answers `[]`. This endpoint defines no query parameters.
pub async fn list(State(state): State<crate::server::AppState>) -> Result<Json<Vec<Pet>>, Failure> {
    let _store = lock_store();
    Ok(Json(load(&store_path(&state))?))
}

/// `POST /v1/services/pets` -> 201 with the created record as the body and
/// `Location: /v1/services/pets/{id}`.
///
/// The header's path is the one the sibling route file registers, written literally because
/// the router hands a handler no route template to derive it from.
pub async fn create(
    State(state): State<crate::server::AppState>,
    Json(input): Json<PetInput>,
) -> Result<Response, Failure> {
    let _store = lock_store();
    let path = store_path(&state);
    let mut pets = load(&path)?;
    let id = next_id(&pets);
    let pet = Pet {
        id,
        name: input.name,
        species: input.species,
        age: input.age,
    };
    pets.push(pet.clone());
    save(&path, &pets)?;
    let response = (
        StatusCode::CREATED,
        [(header::LOCATION, format!("/v1/services/pets/{id}"))],
        Json(pet),
    )
        .into_response();
    Ok(response)
}

/// `GET /v1/services/pets/{id}` -> 200 with `{ "id", "name", "species", "age" }`, or 404 with
/// `{ "error": "no pet with id 7" }`.
///
/// A non-numeric `{id}` never reaches the 404: axum's `Path<u64>` extractor rejects it with
/// 400 before this handler runs, and hand-rolling that check here would only duplicate it.
pub async fn find(
    State(state): State<crate::server::AppState>,
    Path(id): Path<u64>,
) -> Result<Json<Pet>, Failure> {
    let _store = lock_store();
    let pets = load(&store_path(&state))?;
    pets.into_iter()
        .find(|pet| pet.id == id)
        .map(Json)
        .ok_or(Failure::Missing(id))
}

/// `PUT /v1/services/pets/{id}` -> 200 with the replaced record, the path's `id` preserved,
/// or 404 with `{ "error": "no pet with id 7" }`.
///
/// Replacement, not upsert: the body is a [`PetInput`] with no `id`, so a PUT can neither
/// renumber a record nor create a missing one.
pub async fn replace(
    State(state): State<crate::server::AppState>,
    Path(id): Path<u64>,
    Json(input): Json<PetInput>,
) -> Result<Json<Pet>, Failure> {
    let _store = lock_store();
    let path = store_path(&state);
    let mut pets = load(&path)?;
    let index = index_of(&pets, id).ok_or(Failure::Missing(id))?;
    let pet = Pet {
        id,
        name: input.name,
        species: input.species,
        age: input.age,
    };
    pets[index] = pet.clone();
    save(&path, &pets)?;
    Ok(Json(pet))
}

/// `PATCH /v1/services/pets/{id}` -> 200 with the updated record, only the sent fields
/// changed, or 404 with `{ "error": "no pet with id 7" }`.
///
/// `{}` is a valid no-op: it answers 200 with the unchanged record.
pub async fn update(
    State(state): State<crate::server::AppState>,
    Path(id): Path<u64>,
    Json(patch): Json<PetPatch>,
) -> Result<Json<Pet>, Failure> {
    let _store = lock_store();
    let path = store_path(&state);
    let mut pets = load(&path)?;
    let index = index_of(&pets, id).ok_or(Failure::Missing(id))?;
    // Field-by-field merge: `None` (absent key or explicit `null`) keeps the stored value.
    // The unchanged array is still written on a `{}` no-op, which is cheaper than keeping a
    // second, "did anything change" code path in an example.
    let pet = &mut pets[index];
    if let Some(name) = patch.name {
        pet.name = name;
    }
    if let Some(species) = patch.species {
        pet.species = species;
    }
    if let Some(age) = patch.age {
        pet.age = age;
    }
    let merged = pet.clone();
    save(&path, &pets)?;
    Ok(Json(merged))
}

/// `DELETE /v1/services/pets/{id}` -> 204 with an empty body, or 404 with
/// `{ "error": "no pet with id 7" }`.
///
/// 204 carries no body at all, so the deleted record is not echoed, and deleting the same id
/// a second time is therefore a 404. Holes stay in the id sequence - nothing renumbers the
/// survivors - and the next `POST` counts from the highest id still stored.
pub async fn remove(
    State(state): State<crate::server::AppState>,
    Path(id): Path<u64>,
) -> Result<StatusCode, Failure> {
    let _store = lock_store();
    let path = store_path(&state);
    let mut pets = load(&path)?;
    let index = index_of(&pets, id).ok_or(Failure::Missing(id))?;
    pets.remove(index);
    save(&path, &pets)?;
    Ok(StatusCode::NO_CONTENT)
}

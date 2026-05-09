//! Custom `gio::ListModel` backing the library grid.
//!
//! Replaces the previous `gio::ListStore`-of-`AssetObject` mirror. The model
//! owns a `Vec<AssetObject>` reconciled from `LibraryState.assets` (which is
//! itself capped at `WINDOW_CAP` via FIFO eviction). On each apply we emit a
//! single `items_changed(0, prev_n, new_n)` reset.

use std::cell::RefCell;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::api_client::LibraryAsset;
use crate::app_context::AppContext;
use crate::library::asset_object::AssetObject;
use crate::library::state::{LibrarySortMode, PageMutation};

mod imp {
    use super::*;
    use gio::subclass::prelude::ListModelImpl;

    #[derive(Default)]
    pub struct LibraryAssetModel {
        pub items: RefCell<Vec<AssetObject>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LibraryAssetModel {
        const NAME: &'static str = "MimickLibraryAssetModel";
        type Type = super::LibraryAssetModel;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for LibraryAssetModel {}

    impl ListModelImpl for LibraryAssetModel {
        fn item_type(&self) -> glib::Type {
            AssetObject::static_type()
        }

        fn n_items(&self) -> u32 {
            self.items.borrow().len() as u32
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            self.items
                .borrow()
                .get(position as usize)
                .map(|o| o.clone().upcast())
        }
    }
}

glib::wrapper! {
    pub struct LibraryAssetModel(ObjectSubclass<imp::LibraryAssetModel>) @implements gio::ListModel;
}

impl Default for LibraryAssetModel {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryAssetModel {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Replace the model's items with `AssetObject`s built from `assets`,
    /// apply the visual sort, and emit `items_changed(0, prev_n, new_n)`.
    pub fn apply(
        &self,
        ctx: &AppContext,
        assets: &[LibraryAsset],
        sort_mode: &LibrarySortMode,
        mutation: PageMutation,
    ) {
        let new_items = build_sorted_asset_objects(assets, ctx, sort_mode);
        *self.imp().items.borrow_mut() = new_items;
        self.items_changed(0, mutation.prev_n as u32, mutation.new_n as u32);
    }

    /// Replace items wholesale (used for client-side sort changes that don't
    /// produce a `PageMutation`). Emits a `items_changed(0, prev, new)` reset.
    pub fn reset(&self, ctx: &AppContext, assets: &[LibraryAsset], sort_mode: &LibrarySortMode) {
        let prev_n = self.imp().items.borrow().len() as u32;
        let new_items = build_sorted_asset_objects(assets, ctx, sort_mode);
        let new_n = new_items.len() as u32;
        *self.imp().items.borrow_mut() = new_items;
        self.items_changed(0, prev_n, new_n);
    }

    /// Returns the asset id at `position`, if any. Used by handlers that need
    /// position-keyed lookups without going through GObject property access.
    pub fn id_at(&self, position: u32) -> Option<String> {
        self.imp()
            .items
            .borrow()
            .get(position as usize)
            .map(|o| o.property::<String>("id"))
    }

    /// Snapshot the asset-id for every loaded position. Useful for the
    /// timeline scrubber and other consumers that previously indexed
    /// `LibraryState.assets`.
    pub fn created_at_at(&self, position: u32) -> Option<String> {
        self.imp()
            .items
            .borrow()
            .get(position as usize)
            .map(|o| o.property::<String>("created-at"))
    }
}

fn build_sorted_asset_objects(
    assets: &[LibraryAsset],
    ctx: &AppContext,
    sort_mode: &LibrarySortMode,
) -> Vec<AssetObject> {
    let mut items = build_asset_objects(assets, ctx);
    match sort_mode {
        // Server-side date sort: state.assets is already in server order; no
        // client-side reorder. (This applies to `LocalAll`/`AlbumLocal` too —
        // those sources only ever emit one synthetic page so insertion order
        // matches the local enumeration.)
        LibrarySortMode::NewestFirst | LibrarySortMode::OldestFirst => {}
        LibrarySortMode::Filename => items.sort_by_cached_key(|o| {
            (
                o.property::<String>("filename").to_ascii_lowercase(),
                o.property::<String>("id"),
            )
        }),
        LibrarySortMode::FileType => items.sort_by_cached_key(|o| {
            (
                o.property::<String>("mime-type"),
                o.property::<String>("filename"),
                o.property::<String>("id"),
            )
        }),
    }
    items
}

fn build_asset_objects(assets: &[LibraryAsset], ctx: &AppContext) -> Vec<AssetObject> {
    use super::{LOCAL_ID_PREFIX, immich_checksum_to_hex};
    use crate::library::local_source::local_sync_state;

    assets
        .iter()
        .map(|asset| {
            if let Some(local_path) = asset.id.strip_prefix(LOCAL_ID_PREFIX) {
                let sync_state =
                    local_sync_state(&ctx.sync_index, std::path::Path::new(local_path));
                let object = AssetObject::new_local(
                    &asset.id,
                    &asset.filename,
                    &asset.mime_type,
                    &asset.created_at,
                    &asset.asset_type,
                    local_path,
                );
                if sync_state != 1 {
                    object.set_property("sync-state", sync_state);
                }
                return object;
            }
            let local_match = asset
                .checksum
                .as_deref()
                .and_then(immich_checksum_to_hex)
                .as_deref()
                .and_then(|hex| ctx.sync_index.local_path_for_checksum(hex));
            let sync_state = if local_match.is_some() { 2 } else { 0 };
            let object = AssetObject::new(
                &asset.id,
                &asset.filename,
                &asset.mime_type,
                &asset.created_at,
                &asset.asset_type,
                sync_state,
                asset.thumbhash.as_deref(),
            );
            if let Some(path) = local_match {
                object.set_property("local-path", path);
            }
            object
        })
        .collect()
}

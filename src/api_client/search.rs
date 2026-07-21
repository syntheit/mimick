//! Search and explore: smart search, OCR, metadata filters, people, places, server stats.
//!
//! Wraps the Immich `/search` and `/people` endpoints used by the library
//! explore tab and search bar. Supports CLIP-based smart search, OCR text
//! matching, and structured metadata filters with pagination.

use std::time::Duration;

use super::errors::{RequestContext, classify_http_issue, classify_network_issue};
use super::{
    ExifSearchResponse, ExploreSection, ImmichApiClient, LibraryAsset, MapMarker,
    MetadataSearchFilters, PeopleResponse, Person, PlaceItem, SearchResponse, ServerAbout,
    ServerStatistics, ServerStats, SortOrder,
};

impl ImmichApiClient {
    /// Fetch the list of recognized people faces from the server.
    pub async fn fetch_people(&self, include_hidden: bool) -> Result<Vec<Person>, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!(
            "{}/api/people?withHidden={}",
            base_url,
            if include_hidden { "true" } else { "false" }
        );
        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let body = resp
                    .json::<PeopleResponse>()
                    .await
                    .map_err(|err| err.to_string())?;
                self.clear_issue().await;
                Ok(body.people)
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Fetch distinct facet values for a search field from Immich's
    /// `/api/search/suggestions` endpoint. `suggestion_type` is one of the
    /// server's `SearchSuggestionType` values: `camera-make`, `camera-model`,
    /// `city`, `state`, or `country`. Returns a plain `Vec<String>` of values.
    ///
    /// Powers the Camera picker (make/model) and can enrich the Location picker
    /// (country/state/city) without the expensive full metadata scan that
    /// [`fetch_all_places`] performs. Mirrors [`fetch_people`]: a bare GET with
    /// the API key + `Accept: application/json`, HTTP/network errors surfaced as
    /// an `Err(String)` for the caller to handle (pickers fall back to an empty
    /// list rather than blocking).
    pub async fn fetch_search_suggestions(
        &self,
        suggestion_type: &str,
    ) -> Result<Vec<String>, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!(
            "{}/api/search/suggestions?type={}",
            base_url, suggestion_type
        );
        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let mut values = resp
                    .json::<Vec<String>>()
                    .await
                    .map_err(|err| err.to_string())?;
                // The server may include null/empty entries for assets missing
                // the field; drop them so the picker list is clean.
                values.retain(|v| !v.trim().is_empty());
                self.clear_issue().await;
                Ok(values)
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Sectioned tile data (places + things) for the Explore landing.
    pub async fn fetch_explore(&self) -> Result<Vec<ExploreSection>, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/search/explore", base_url);
        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let sections = resp
                    .json::<Vec<ExploreSection>>()
                    .await
                    .map_err(|err| err.to_string())?;
                self.clear_issue().await;
                Ok(sections)
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Fetch unique cities that have at least one asset with EXIF city data.
    /// Pages through `/api/search/metadata` collecting one representative asset
    /// per city.
    ///
    /// The scan is **bounded** so the Places section always resolves to content
    /// or an empty state rather than spinning while a whole huge library is
    /// walked: it stops at `MAX_PAGES` pages or once `TIME_BUDGET` elapses,
    /// whichever comes first. A mid-scan network/parse error returns the cities
    /// gathered so far (partial results) instead of discarding everything and
    /// leaving the section blank — the goal is "never an infinite spinner",
    /// and a partial city list is strictly better than none.
    pub async fn fetch_all_places(&self) -> Result<Vec<PlaceItem>, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/search/metadata", base_url);

        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut page: u32 = 1;
        let mut completed_pages: u32 = 0;
        const PAGE_SIZE: u32 = 250;
        // Bounded scan: cap the number of pages and the wall-clock time so the
        // spinner can never spin indefinitely on a very large library. 40 pages
        // × 250 = up to 10k assets sampled, which surfaces essentially every
        // distinct city in practice while keeping first paint quick.
        const MAX_PAGES: u32 = 40;
        const TIME_BUDGET: Duration = Duration::from_secs(12);
        let start = std::time::Instant::now();

        loop {
            let body = serde_json::json!({
                "withExif": true,
                "page": page,
                "size": PAGE_SIZE,
            });
            let resp = match self
                .client
                .post(&url)
                .header("x-api-key", &settings.api_key)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(&body)
                .timeout(Duration::from_secs(30))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                // A hard auth error on the very first page has no partial result
                // to salvage — propagate it so the caller can surface the
                // permission dialog. Otherwise keep what we have.
                Ok(r) => {
                    if page == 1 {
                        return Err(format!("HTTP {}", r.status()));
                    }
                    log::warn!("fetch_all_places: stopping at page {page} (HTTP {})", r.status());
                    break;
                }
                Err(e) => {
                    if page == 1 {
                        return Err(e.to_string());
                    }
                    log::warn!("fetch_all_places: stopping at page {page} ({e})");
                    break;
                }
            };
            let parsed: ExifSearchResponse = match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    if page == 1 {
                        return Err(e.to_string());
                    }
                    log::warn!("fetch_all_places: parse error at page {page} ({e})");
                    break;
                }
            };
            let has_more = parsed.assets.next_page.is_some();
            for asset in parsed.assets.items {
                if let Some(city) = asset.exif_info.as_ref().and_then(|e| e.city.clone()) {
                    seen.entry(city).or_insert(asset.id);
                }
            }
            completed_pages += 1;
            if !has_more || page >= MAX_PAGES || start.elapsed() >= TIME_BUDGET {
                break;
            }
            page += 1;
        }

        let mut places: Vec<PlaceItem> = seen
            .into_iter()
            .map(|(city, asset_id)| PlaceItem { city, asset_id })
            .collect();
        places.sort_by(|a, b| a.city.cmp(&b.city));
        log::debug!(
            "fetch_all_places: {} cities from {} pages in {:.1}s",
            places.len(),
            completed_pages,
            start.elapsed().as_secs_f64()
        );
        Ok(places)
    }

    /// Fetch every geotagged asset's map marker from Immich's `/api/map/markers`
    /// endpoint. Powers the browsable Places map: one `MapMarker` (asset id +
    /// lat/lon) per photo/video that has GPS coordinates.
    ///
    /// A single bare GET returning the full array — the server computes the set
    /// server-side, so unlike [`fetch_all_places`] there is no pagination to
    /// walk. Mirrors [`fetch_people`]'s header/timeout/error shape (API key +
    /// `Accept: application/json`, HTTP/network errors surfaced as `Err(String)`
    /// for the caller to turn into an empty-state). The timeout is generous
    /// because a large library can return tens of thousands of markers.
    pub async fn fetch_map_markers(&self) -> Result<Vec<MapMarker>, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/map/markers", base_url);
        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let markers = resp
                    .json::<Vec<MapMarker>>()
                    .await
                    .map_err(|err| err.to_string())?;
                self.clear_issue().await;
                log::debug!("fetch_map_markers: {} markers", markers.len());
                Ok(markers)
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Per-person face thumbnail. Distinct from `fetch_thumbnail` (asset).
    pub async fn fetch_person_thumbnail(&self, person_id: &str) -> Result<Vec<u8>, String> {
        let _permit = self
            .thumbnail_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| err.to_string())?;
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/people/{}/thumbnail", base_url, person_id);
        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/octet-stream")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().await.map_err(|err| err.to_string())?;
                Ok(bytes.to_vec())
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Perform a CLIP embedding-based smart search for matching assets.
    pub async fn search_smart(
        &self,
        query: &str,
        page: u32,
        size: u32,
    ) -> Result<(Vec<LibraryAsset>, bool), String> {
        let body = serde_json::json!({
            "query": query,
            "page": page,
            "size": size.max(1),
        });
        self.fetch_search_assets(
            "/api/search/smart",
            body,
            RequestContext::SmartSearch,
            Some(query),
        )
        .await
    }

    /// Perform an OCR-based search to match recognized text inside library images.
    pub async fn search_ocr(
        &self,
        query: &str,
        page: u32,
        size: u32,
        order: Option<SortOrder>,
    ) -> Result<(Vec<LibraryAsset>, bool), String> {
        let mut body = serde_json::json!({
            "ocr": query,
            "page": page,
            "size": size.max(1),
        });
        if let Some(order) = order
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert(
                "order".into(),
                serde_json::to_value(order).unwrap_or(serde_json::Value::Null),
            );
        }
        self.fetch_search_assets(
            "/api/search/metadata",
            body,
            RequestContext::MetadataSearch,
            Some(query),
        )
        .await
    }

    /// Search library assets matching specific text within their original filenames.
    pub async fn search_metadata(
        &self,
        query: &str,
        page: u32,
        size: u32,
        order: Option<SortOrder>,
    ) -> Result<(Vec<LibraryAsset>, bool), String> {
        let filters = MetadataSearchFilters {
            original_file_name: Some(query.to_string()),
            order,
            ..Default::default()
        };
        self.search_metadata_with_filters(&filters, page, size)
            .await
    }

    /// Perform a advanced search matching specific metadata filters.
    pub async fn search_metadata_with_filters(
        &self,
        filters: &MetadataSearchFilters,
        page: u32,
        size: u32,
    ) -> Result<(Vec<LibraryAsset>, bool), String> {
        let mut body = serde_json::to_value(filters).map_err(|err| err.to_string())?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("page".into(), serde_json::json!(page));
            obj.insert("size".into(), serde_json::json!(size.max(1)));
        }
        let label = filters.original_file_name.as_deref().unwrap_or("");
        self.fetch_search_assets(
            "/api/search/metadata",
            body,
            RequestContext::MetadataSearch,
            Some(label),
        )
        .await
    }

    /// Count how many assets match `filters` without fetching the full result
    /// set. Requests a single-item page and reads Immich's `total` field from
    /// the `/search/metadata` response.
    ///
    /// Powers the Filters sheet's live "Show N photos" apply button. Returns:
    /// - `Ok(Some(n))` — the server reported an exact `total`.
    /// - `Ok(None)`   — the request succeeded but the server omitted `total`
    ///   (older/edge servers); the caller shows a neutral "Show photos" label
    ///   rather than a guessed number.
    /// - `Err(_)`     — network/HTTP failure; the caller keeps the last known
    ///   label. Deliberately does *not* mutate the connection issue state, so a
    ///   transient count probe never trips the reconnect/permission machinery.
    pub async fn count_metadata_matches(
        &self,
        filters: &MetadataSearchFilters,
    ) -> Result<Option<u64>, String> {
        let mut body = serde_json::to_value(filters).map_err(|err| err.to_string())?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("page".into(), serde_json::json!(1));
            obj.insert("size".into(), serde_json::json!(1));
            // Match the timeline-scope default that `fetch_search_assets`
            // applies, so the count reflects the same result set the drill-in
            // will show (hidden Live-Photo movie parts excluded, etc.).
            if !obj.contains_key("visibility")
                && !obj.contains_key("isArchived")
                && !obj.contains_key("withDeleted")
                && !obj.contains_key("isTrashed")
            {
                obj.insert(
                    "visibility".into(),
                    serde_json::Value::String("timeline".into()),
                );
            }
            obj.entry("withExif")
                .or_insert(serde_json::Value::Bool(true));
        }

        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/search/metadata", base_url);

        match self
            .client
            .post(&url)
            .header("x-api-key", &settings.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let response = resp
                    .json::<SearchResponse>()
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(response.assets.total)
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Retrieve total images, videos, and overall asset count statistics from the server.
    pub async fn fetch_server_stats(&self) -> Result<ServerStats, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/assets/statistics", base_url);

        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let stats = resp
                    .json::<ServerStats>()
                    .await
                    .map_err(|err| err.to_string())?;
                self.clear_issue().await;
                Ok(stats)
            }
            Ok(resp) => {
                self.set_issue(classify_http_issue(
                    RequestContext::ServerStats,
                    resp.status().as_u16(),
                    None,
                ))
                .await;
                Err(format!("HTTP {}", resp.status()))
            }
            Err(err) => {
                *self.active_url.lock().await = None;
                self.set_issue(classify_network_issue(RequestContext::ServerStats, &err))
                    .await;
                Err(err.to_string())
            }
        }
    }

    /// Fetch server-wide statistics including per-user usage breakdown.
    ///
    /// Admin-only endpoint; non-admin sessions will receive an HTTP 403 and an
    /// `Err` is returned. Caller should fall back to per-user asset counts.
    pub async fn fetch_server_statistics(&self) -> Result<ServerStatistics, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/server/statistics", base_url);

        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp
                .json::<ServerStatistics>()
                .await
                .map_err(|err| err.to_string()),
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Retrieve detailed Immich server system information (e.g. version).
    pub async fn fetch_server_about(&self) -> Result<ServerAbout, String> {
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}/api/server/about", base_url);

        match self
            .client
            .get(&url)
            .header("x-api-key", &settings.api_key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let about = resp
                    .json::<ServerAbout>()
                    .await
                    .map_err(|err| err.to_string())?;
                self.clear_issue().await;
                Ok(about)
            }
            Ok(resp) => {
                self.set_issue(classify_http_issue(
                    RequestContext::ServerAbout,
                    resp.status().as_u16(),
                    None,
                ))
                .await;
                Err(format!("HTTP {}", resp.status()))
            }
            Err(err) => {
                *self.active_url.lock().await = None;
                self.set_issue(classify_network_issue(RequestContext::ServerAbout, &err))
                    .await;
                Err(err.to_string())
            }
        }
    }

    /// Shared internal helper executing paginated POST search queries against asset endpoints.
    pub(super) async fn fetch_search_assets(
        &self,
        endpoint: &str,
        mut body: serde_json::Value,
        context: RequestContext,
        subject: Option<&str>,
    ) -> Result<(Vec<LibraryAsset>, bool), String> {
        if let Some(obj) = body.as_object_mut() {
            obj.entry("withExif")
                .or_insert(serde_json::Value::Bool(true));
            // Exclude hidden assets (e.g. the .MOV motion parts of iPhone Live
            // Photos, which Immich marks visibility=hidden and never generates
            // thumbnails for — they'd otherwise show as blank tiles / 404s).
            // Only default to the timeline scope when the caller hasn't asked
            // for a different one (archive/trash set their own filters).
            if !obj.contains_key("visibility")
                && !obj.contains_key("isArchived")
                && !obj.contains_key("withDeleted")
                && !obj.contains_key("isTrashed")
            {
                obj.insert(
                    "visibility".into(),
                    serde_json::Value::String("timeline".into()),
                );
            }
        }
        let base_url = self
            .get_active_url()
            .await
            .ok_or_else(|| "No active connection".to_string())?;
        let settings = self.settings_snapshot();
        let url = format!("{}{}", base_url, endpoint);

        match self
            .client
            .post(&url)
            .header("x-api-key", &settings.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let response = resp
                    .json::<SearchResponse>()
                    .await
                    .map_err(|err| err.to_string())?;
                self.clear_issue().await;
                let has_more = response.assets.next_page.is_some();
                Ok((response.assets.items, has_more))
            }
            Ok(resp) => {
                self.set_issue(classify_http_issue(
                    context,
                    resp.status().as_u16(),
                    subject,
                ))
                .await;
                Err(format!("HTTP {}", resp.status()))
            }
            Err(err) => {
                *self.active_url.lock().await = None;
                self.set_issue(classify_network_issue(context, &err)).await;
                Err(err.to_string())
            }
        }
    }
}

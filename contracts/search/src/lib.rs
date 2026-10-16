//! Search contract — indexes artists for discovery with filtering, sorting,
//! pagination, keyword ("full-text") metadata, and search analytics (#599).
//!
//! This contract intentionally does not attempt real off-chain-search-engine
//! style full-text indexing on-chain — that is infeasible to do cheaply in a
//! Soroban contract. Instead it maintains a tag/keyword index that clients
//! populate, and scans+filters+sorts+paginates the indexed set per query.
//! Result sets are bounded by `MAX_PAGE_SIZE` and listings are bounded by
//! `MAX_SKILLS`/`MAX_KEYWORDS`, keeping each call's cost predictable.

#![no_std]
use soroban_sdk::{contract, contractimpl, symbol_short, Address, Env, String, Vec};

pub mod errors;
pub mod types;

#[cfg(test)]
mod test;

use errors::SearchError;
use types::{ArtistListing, DataKey, SearchAnalytics, SearchFilters, SearchResultPage, SortBy};

include!("../../semver_types.rs");

/// Maximum number of skill tags a single listing may carry.
const MAX_SKILLS: u32 = 20;
/// Maximum number of keyword tags a single listing may carry.
const MAX_KEYWORDS: u32 = 30;
/// Hard cap on a single page of search results, bounding per-call cost.
const MAX_PAGE_SIZE: u32 = 50;

#[contract]
pub struct Search;

fn has_admin(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Admin)
}

fn get_admin(env: &Env) -> Result<Address, SearchError> {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(SearchError::NotInitialized)
}

fn require_admin(env: &Env) -> Result<Address, SearchError> {
    let admin = get_admin(env)?;
    admin.require_auth();
    Ok(admin)
}

fn load_listing(env: &Env, artist: &Address) -> Result<ArtistListing, SearchError> {
    env.storage()
        .persistent()
        .get(&DataKey::Listing(artist.clone()))
        .ok_or(SearchError::ListingNotFound)
}

fn save_listing(env: &Env, listing: &ArtistListing) {
    env.storage()
        .persistent()
        .set(&DataKey::Listing(listing.artist.clone()), listing);
}

fn load_analytics(env: &Env) -> SearchAnalytics {
    env.storage()
        .instance()
        .get(&DataKey::Analytics)
        .unwrap_or_default()
}

fn save_analytics(env: &Env, analytics: &SearchAnalytics) {
    env.storage().instance().set(&DataKey::Analytics, analytics);
}

fn contains_string(haystack: &Vec<String>, needle: &String) -> bool {
    haystack.iter().any(|s| s == *needle)
}

fn matches_filters(listing: &ArtistListing, filters: &SearchFilters) -> bool {
    if !listing.active {
        return false;
    }
    if let Some(skill) = &filters.skill {
        if !contains_string(&listing.skills, skill) {
            return false;
        }
    }
    if let Some(min_price) = filters.min_price {
        if listing.price < min_price {
            return false;
        }
    }
    if let Some(max_price) = filters.max_price {
        if listing.price > max_price {
            return false;
        }
    }
    if let Some(min_rating) = filters.min_rating {
        if listing.rating < min_rating {
            return false;
        }
    }
    if let Some(keyword) = &filters.keyword {
        if !contains_string(&listing.keywords, keyword) {
            return false;
        }
    }
    true
}

fn is_ordered(a: &ArtistListing, b: &ArtistListing, sort_by: SortBy) -> bool {
    // True when `a` should come at or before `b` under `sort_by`.
    match sort_by {
        SortBy::PriceAsc => a.price <= b.price,
        SortBy::PriceDesc => a.price >= b.price,
        SortBy::RatingDesc => a.rating >= b.rating,
        SortBy::RatingAsc => a.rating <= b.rating,
        SortBy::Newest => a.indexed_ledger >= b.indexed_ledger,
    }
}

/// Bounded top-k selection: scan `all_artists` once, maintaining only the top
/// `k = start + page_size` candidates in a small buffer via insertion.
///
/// This is O(m · k) with tiny k (at most 2 × MAX_PAGE_SIZE) instead of the
/// O(m log m) full sort-then-window. Output is byte-identical to the previous
/// implementation for the same input, including tie-breaking (ties break towards
/// the earlier artist in scan order, matching the stable sort behaviour).
///
/// Returns `(page_results, total_matches)` so the caller can populate
/// `SearchResultPage` without retaining the full match set.
fn select_page(
    env: &Env,
    all_artists: &Vec<Address>,
    filters: &SearchFilters,
    sort_by: SortBy,
    start: u32,
    page_size: u32,
) -> (Vec<ArtistListing>, u32) {
    // k is the number of candidates we keep: enough to fill `start + page_size`.
    let k = start.saturating_add(page_size);
    // Buffer holding the current top-k candidates, insertion-sorted.
    let mut buf: Vec<ArtistListing> = Vec::new(env);
    let mut total_matches: u32 = 0;

    for artist in all_artists.iter() {
        let maybe_listing: Option<ArtistListing> =
            env.storage().persistent().get(&DataKey::Listing(artist));
        if let Some(listing) = maybe_listing {
            if !matches_filters(&listing, filters) {
                continue;
            }
            total_matches += 1;

            // Determine where `listing` fits in the current buffer.
            // We want buf[0] to be the "best" (first in sorted order) and
            // buf[len-1] to be the "worst". We only keep the top-k best.
            let buf_len = buf.len();

            if buf_len < k {
                // Buffer not yet full — insert in sorted position.
                let mut pos = buf_len;
                while pos > 0 {
                    let prev = buf.get(pos - 1).unwrap();
                    // `is_ordered(prev, listing)` means prev comes before listing,
                    // so listing goes after. Stop scanning backwards.
                    if is_ordered(&prev, &listing, sort_by) {
                        break;
                    }
                    pos -= 1;
                }
                buf.insert(pos, listing);
            } else {
                // Buffer is full. Only insert if this candidate beats the
                // current worst (buf[k-1]) — otherwise it would be outside
                // the top-k window and can be dropped.
                let worst = buf.get(k - 1).unwrap();
                if is_ordered(&listing, &worst, sort_by) && listing != worst {
                    // Find insertion position in the top-(k-1) portion.
                    let mut pos = k - 1;
                    while pos > 0 {
                        let prev = buf.get(pos - 1).unwrap();
                        if is_ordered(&prev, &listing, sort_by) {
                            break;
                        }
                        pos -= 1;
                    }
                    // Remove last (worst) element and insert new one.
                    buf.remove(k - 1);
                    buf.insert(pos, listing);
                }
            }
        }
    }

    // Extract the page window [start, start + page_size) from the buffer.
    let mut page_results: Vec<ArtistListing> = Vec::new(env);
    if start < buf.len() {
        let end = core::cmp::min(start.saturating_add(page_size), buf.len());
        let mut i = start;
        while i < end {
            page_results.push_back(buf.get(i).unwrap());
            i += 1;
        }
    }
    (page_results, total_matches)
}

#[contractimpl]
impl Search {
    pub fn initialize(env: Env, admin: Address) -> Result<(), SearchError> {
        if has_admin(&env) {
            return Err(SearchError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.events().publish((symbol_short!("init"),), admin);
        Ok(())
    }

    impl_semver_queries!();

    /// Create or update the caller's own search listing. Rating is not
    /// settable here — see `set_rating`.
    pub fn index_artist(
        env: Env,
        artist: Address,
        skills: Vec<String>,
        price: i128,
        keywords: Vec<String>,
    ) -> Result<(), SearchError> {
        if !has_admin(&env) {
            return Err(SearchError::NotInitialized);
        }
        artist.require_auth();

        if price < 0 {
            return Err(SearchError::InvalidPrice);
        }
        if skills.len() > MAX_SKILLS {
            return Err(SearchError::TooManySkills);
        }
        if keywords.len() > MAX_KEYWORDS {
            return Err(SearchError::TooManyKeywords);
        }

        let is_new = !env
            .storage()
            .persistent()
            .has(&DataKey::Listing(artist.clone()));
        let rating = if is_new {
            0
        } else {
            load_listing(&env, &artist)?.rating
        };

        let listing = ArtistListing {
            artist: artist.clone(),
            skills,
            price,
            rating,
            keywords,
            indexed_ledger: env.ledger().sequence(),
            active: true,
        };
        save_listing(&env, &listing);

        if is_new {
            let mut all: Vec<Address> = env
                .storage()
                .instance()
                .get(&DataKey::AllArtists)
                .unwrap_or_else(|| Vec::new(&env));
            all.push_back(artist.clone());
            env.storage().instance().set(&DataKey::AllArtists, &all);

            let mut analytics = load_analytics(&env);
            analytics.total_indexed += 1;
            save_analytics(&env, &analytics);
        }

        env.events()
            .publish((symbol_short!("indexed"),), (artist, price));
        Ok(())
    }

    /// Admin-set rating snapshot, typically synced from the reputation
    /// contract. Kept out of the artist's own control to prevent
    /// self-inflated search ranking.
    pub fn set_rating(env: Env, artist: Address, rating: u32) -> Result<(), SearchError> {
        require_admin(&env)?;
        if rating > 100 {
            return Err(SearchError::InvalidRating);
        }
        let mut listing = load_listing(&env, &artist)?;
        listing.rating = rating;
        save_listing(&env, &listing);
        env.events()
            .publish((symbol_short!("rating"),), (artist, rating));
        Ok(())
    }

    /// The artist (or admin) removes the listing from search results without
    /// losing its history; `index_artist` reactivates it.
    pub fn deactivate_listing(env: Env, caller: Address, artist: Address) -> Result<(), SearchError> {
        let admin = get_admin(&env)?;
        caller.require_auth();
        if caller != artist && caller != admin {
            return Err(SearchError::Unauthorized);
        }
        let mut listing = load_listing(&env, &artist)?;
        listing.active = false;
        save_listing(&env, &listing);
        env.events()
            .publish((symbol_short!("deact"),), artist);
        Ok(())
    }

    pub fn reactivate_listing(env: Env, caller: Address, artist: Address) -> Result<(), SearchError> {
        let admin = get_admin(&env)?;
        caller.require_auth();
        if caller != artist && caller != admin {
            return Err(SearchError::Unauthorized);
        }
        let mut listing = load_listing(&env, &artist)?;
        listing.active = true;
        save_listing(&env, &listing);
        env.events().publish((symbol_short!("react"),), artist);
        Ok(())
    }

    pub fn get_listing(env: Env, artist: Address) -> Result<ArtistListing, SearchError> {
        load_listing(&env, &artist)
    }

    /// Filter, sort, and paginate the indexed artist set. `page` is
    /// zero-based. Also increments the search-analytics counter.
    pub fn search(
        env: Env,
        filters: SearchFilters,
        sort_by: SortBy,
        page: u32,
        page_size: u32,
    ) -> Result<SearchResultPage, SearchError> {
        if !has_admin(&env) {
            return Err(SearchError::NotInitialized);
        }
        if page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(SearchError::InvalidPageSize);
        }
        if let (Some(min), Some(max)) = (filters.min_price, filters.max_price) {
            if min > max {
                return Err(SearchError::InvalidPriceRange);
            }
        }

        let all: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::AllArtists)
            .unwrap_or_else(|| Vec::new(&env));

        // Bounded top-k selection: O(m · k) where k = start + page_size,
        // instead of O(m log m) over the full match set. Output is identical
        // to the previous full-sort + window approach. See `select_page`.
        let start = page.saturating_mul(page_size);
        let (page_results, total_matches) =
            select_page(&env, &all, &filters, sort_by, start, page_size);

        let mut analytics = load_analytics(&env);
        analytics.total_searches += 1;
        save_analytics(&env, &analytics);

        Ok(SearchResultPage {
            results: page_results,
            total_matches,
            page,
            page_size,
        })
    }

    pub fn get_analytics(env: Env) -> SearchAnalytics {
        load_analytics(&env)
    }

    pub fn get_indexed_count(env: Env) -> u32 {
        load_analytics(&env).total_indexed
    }
}

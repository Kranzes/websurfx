//! This module handles the search route of the search engine website.

use crate::{
    cache::cacher::SharedCache,
    config::Config,
    handler::{file_path, FileType},
    models::{
        aggregation_models::SearchResults,
        engine_models::EngineHandler,
        server_models::{self, SearchParams},
    },
    results::aggregator::aggregate,
};
use actix_web::{get, http::header::ContentType, web, HttpRequest, HttpResponse};
use regex::Regex;
use std::{
    borrow::Cow,
    fs::File,
    io::{BufRead, BufReader, Read},
};
use tokio::join;

/// Handles the route of search page of the `websurfx` meta search engine website and it takes
/// two search url parameters `q` and `page` where `page` parameter is optional.
///
/// # Example
///
/// ```bash
/// curl "http://127.0.0.1:8080/search?q=sweden&page=1"
/// ```
///
/// Or
///
/// ```bash
/// curl "http://127.0.0.1:8080/search?q=sweden"
/// ```
#[get("/search")]
pub async fn search(
    req: HttpRequest,
    config: web::Data<Config>,
    cache: web::Data<SharedCache>,
) -> Result<HttpResponse, Box<dyn std::error::Error>> {
    use std::sync::Arc;
    let params = web::Query::<SearchParams>::from_query(req.query_string())?;
    match &params.q {
        Some(query) => {
            if query.trim().is_empty() {
                return Ok(HttpResponse::TemporaryRedirect()
                    .insert_header(("location", "/"))
                    .finish());
            }

            let cookie = req.cookie("appCookie");

            // Get search settings using the user's cookie or from the server's config
            let mut search_settings: server_models::Cookie<'_> = cookie
                .and_then(|cookie_value| serde_json::from_str(cookie_value.value()).ok())
                .unwrap_or_else(|| {
                    server_models::Cookie::build(
                        &config.style,
                        config
                            .search
                            .upstream_search_engines
                            .iter()
                            .filter_map(|(engine, enabled)| {
                                enabled.then_some(Cow::Borrowed(engine.as_str()))
                            })
                            .collect(),
                        config.search.safe_search,
                    )
                });

            search_settings.safe_search_level = get_safesearch_level(
                &Some(search_settings.safe_search_level),
                &params.safesearch,
                config.search.safe_search,
            );

            // Closure wrapping the results function capturing local references
            let get_results = |page| results(&config, &cache, query, page, &search_settings);

            // .max(1) makes sure that the page >= 0.
            let page = params.page.unwrap_or(1).max(1) - 1;
            let previous_page = page.saturating_sub(1);
            let next_page = page + 1;

            let mut results = Arc::new((SearchResults::default(), String::default()));
            if page != previous_page {
                let (previous_results, current_results, next_results) = join!(
                    get_results(previous_page),
                    get_results(page),
                    get_results(next_page)
                );
                let (parsed_previous_results, parsed_next_results) =
                    (previous_results?, next_results?);

                let (cache_keys, results_list) = (
                    [
                        parsed_previous_results.1,
                        results.1.clone(),
                        parsed_next_results.1,
                    ],
                    [
                        parsed_previous_results.0,
                        results.0.clone(),
                        parsed_next_results.0,
                    ],
                );

                results = Arc::new(current_results?);

                tokio::spawn(async move { cache.cache_results(&results_list, &cache_keys).await });
            } else {
                let (current_results, next_results) =
                    join!(get_results(page), get_results(page + 1));

                let parsed_next_results = next_results?;

                results = Arc::new(current_results?);

                let (cache_keys, results_list) = (
                    [results.1.clone(), parsed_next_results.1.clone()],
                    [results.0.clone(), parsed_next_results.0],
                );

                tokio::spawn(async move { cache.cache_results(&results_list, &cache_keys).await });
            }

            Ok(HttpResponse::Ok().content_type(ContentType::html()).body(
                crate::templates::views::search::search(
                    &config.style.colorscheme,
                    &config.style.theme,
                    &config.style.animation,
                    query,
                    &results.0,
                )
                .0,
            ))
        }
        None => Ok(HttpResponse::TemporaryRedirect()
            .insert_header(("location", "/"))
            .finish()),
    }
}

/// Fetches the results for a query and page. It First checks the redis cache, if that
/// fails it gets proper results by requesting from the upstream search engines.
///
/// # Arguments
///
/// * `url` - It takes the url of the current page that requested the search results for a
/// particular search query.
/// * `config` - It takes a parsed config struct.
/// * `query` - It takes the page number as u32 value.
/// * `req` - It takes the `HttpRequest` struct as a value.
///
/// # Error
///
/// It returns the `SearchResults` struct if the search results could be successfully fetched from
/// the cache or from the upstream search engines otherwise it returns an appropriate error.
async fn results(
    config: &Config,
    cache: &web::Data<SharedCache>,
    query: &str,
    page: u32,
    search_settings: &server_models::Cookie<'_>,
) -> Result<(SearchResults, String), Box<dyn std::error::Error>> {
    // eagerly parse cookie value to evaluate safe search level
    let safe_search_level = search_settings.safe_search_level;

    let cache_key = format!(
        "http://{}:{}/search?q={}&page={}&safesearch={}&engines={}",
        config.server.binding_ip,
        config.server.port,
        query,
        page,
        safe_search_level,
        search_settings.engines.join(",")
    );

    // fetch the cached results json.
    let cached_results = cache.cached_results(&cache_key).await;
    // check if fetched cache results was indeed fetched or it was an error and if so
    // handle the data accordingly.
    match cached_results {
        Ok(results) => Ok((results, cache_key)),
        Err(_) => {
            if safe_search_level == 4 {
                let mut results: SearchResults = SearchResults::default();

                let flag: bool =
                    !is_match_from_filter_list(file_path(FileType::BlockList)?, query)?;
                // Return early when query contains disallowed words,
                if flag {
                    results.set_disallowed();
                    cache
                        .cache_results(&[results.clone()], &[cache_key.clone()])
                        .await?;
                    results.set_safe_search_level(safe_search_level);
                    return Ok((results, cache_key));
                }
            }

            // check if the cookie value is empty or not if it is empty then use the
            // default selected upstream search engines from the config file otherwise
            // parse the non-empty cookie and grab the user selected engines from the
            // UI and use that.
            let mut results: SearchResults = match search_settings.engines.is_empty() {
                false => {
                    aggregate(
                        query,
                        page,
                        config.server.aggregator.random_delay,
                        config.server.debug,
                        &search_settings
                            .engines
                            .iter()
                            .filter_map(|engine| EngineHandler::new(engine).ok())
                            .collect::<Vec<EngineHandler>>(),
                        config.server.request_timeout,
                        safe_search_level,
                    )
                    .await?
                }
                true => {
                    let mut search_results = SearchResults::default();
                    search_results.set_no_engines_selected();
                    search_results
                }
            };
            if results.engine_errors_info().is_empty()
                && results.results().is_empty()
                && !results.no_engines_selected()
            {
                results.set_filtered();
            }
            cache
                .cache_results(&[results.clone()], &[cache_key.clone()])
                .await?;
            results.set_safe_search_level(safe_search_level);
            Ok((results, cache_key))
        }
    }
}

/// A helper function which checks whether the search query contains any keywords which should be
/// disallowed/allowed based on the regex based rules present in the blocklist and allowlist files.
///
/// # Arguments
///
/// * `file_path` - It takes the file path of the list as the argument.
/// * `query` - It takes the search query to be checked against the list as an argument.
///
/// # Error
///
/// Returns a bool indicating whether the results were found in the list or not on success
/// otherwise returns a standard error type on a failure.
fn is_match_from_filter_list(
    file_path: &str,
    query: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(File::open(file_path)?);
    for line in reader.by_ref().lines() {
        let re = Regex::new(&line?)?;
        if re.is_match(query) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// A helper function to modify the safe search level based on the url params.
/// The `safe_search` is the one in the user's cookie or
/// the default set by the server config if the cookie was missing.
///
/// # Argurments
///
/// * `url_level` - Safe search level from the url.
/// * `safe_search` - User's cookie, or the safe search level set by the server
/// * `config_level` - Safe search level to fall back to
fn get_safesearch_level(cookie_level: &Option<u8>, url_level: &Option<u8>, config_level: u8) -> u8 {
    match url_level {
        Some(url_level) => {
            if *url_level >= 3 {
                config_level
            } else {
                *url_level
            }
        }
        None => cookie_level.unwrap_or(config_level),
    }
}

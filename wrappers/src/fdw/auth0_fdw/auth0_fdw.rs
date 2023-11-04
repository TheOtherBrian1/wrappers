use super::result::Auth0Response;
use super::{Auth0FdwError, Auth0FdwResult};
use crate::stats;
use pgrx::notice;
use pgrx::pg_sys::panic::ErrorReport;
use pgrx::PgSqlErrorCode;
use reqwest::{self, header};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use supabase_wrappers::prelude::*;
use url::Url;

#[derive(Deserialize, Debug)]
pub struct Auth0Rec {
    pub created_at: String,
}
// A simple demo FDW
#[wrappers_fdw(
    version = "0.1.1",
    author = "Joel",
    website = "https://github.com/supabase/wrappers/tree/main/wrappers/src/fdw/auth0_fdw",
    error_type = "Auth0FdwError"
)]
pub(crate) struct Auth0Fdw {
    // row counter
    rt: Runtime,
    client: Option<ClientWithMiddleware>,
    base_url: String,
    scan_result: Option<Vec<Row>>,

    // target column list
    tgt_cols: Vec<Column>,
}

fn create_client(api_key: &str) -> Result<ClientWithMiddleware, Auth0FdwError> {
    let mut headers = header::HeaderMap::new();
    let value = format!("Bearer {}", api_key);
    let mut auth_value =
        header::HeaderValue::from_str(&value).map_err(|_| Auth0FdwError::InvalidApiKeyHeader)?;
    auth_value.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, auth_value);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    Ok(ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build())
}

impl Auth0Fdw {
    const FDW_NAME: &str = "Auth0Fdw";

    fn set_limit_offset(
        &self,
        url: &str,
        page_size: Option<usize>,
        offset: Option<&str>,
    ) -> Auth0FdwResult<String> {
        let mut params = Vec::new();
        if let Some(page_size) = page_size {
            params.push(("pageSize", format!("{}", page_size)));
        }
        if let Some(offset) = offset {
            params.push(("offset", offset.to_string()));
        }

        Ok(Url::parse_with_params(url, &params).map(|x| x.into())?)
    }
    // convert response botext to rows
    fn parse_resp(
        &self,
        resp_body: &str,
        columns: &[Column],
    ) -> Auth0FdwResult<(Vec<Row>, Option<String>)> {
        notice!("This is response body: {:?}", resp_body);
        let response: Vec<Auth0Rec> = serde_json::from_str(resp_body)?;
        notice!("This is response : {:?}", response);
        let mut result = Vec::new();

        // for record in response.records.iter() {
        //     result.push(record.to_row(columns)?);
        // }
        notice!("This is result {:?}", result);

        Ok((result, None))
    }
}

impl ForeignDataWrapper<Auth0FdwError> for Auth0Fdw {
    // 'options' is the key-value pairs defined in `CREATE SERVER` SQL, for example,
    //
    // create server my_auth0_server
    //   foreign data wrapper wrappers_auth0
    //   options (
    //     foo 'bar'
    // );
    //
    // 'options' passed here will be a hashmap { 'foo' -> 'bar' }.
    //
    // You can do any initalization in this new() function, like saving connection
    // info or API url in an variable, but don't do any heavy works like making a
    // database connection or API call.

    fn new(options: &HashMap<String, String>) -> Auth0FdwResult<Self> {
        notice!("Test this app");
        let base_url = options
            .get("api_url")
            .map(|t| t.to_owned())
            // TODO: Find a way to pass through tenant
            // TODO: Replace this with tenant string
            .unwrap_or_else(|| {
                "https://dev-rtoursnfpxmjl0hz.us.auth0.com/api/v2/users".to_string()
            });

        let client = match options.get("api_key") {
            Some(api_key) => Some(create_client(api_key)?),
            None => {
                let key_id = require_option("api_key_id", options)?;
                if let Some(api_key) = get_vault_secret(key_id) {
                    Some(create_client(&api_key)?)
                } else {
                    None
                }
            }
        };
        stats::inc_stats(Self::FDW_NAME, stats::Metric::CreateTimes, 1);
        Ok(Self {
            rt: create_async_runtime()?,
            base_url: base_url,
            client: client,
            tgt_cols: Vec::new(),
            scan_result: None,
        })
    }

    fn begin_scan(
        &mut self,
        _quals: &[Qual],
        columns: &[Column],
        _sorts: &[Sort],
        _limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) -> Auth0FdwResult<()> {
        // save a copy of target columns
        self.tgt_cols = columns.to_vec();
        let mut rows = Vec::new();
        // TODO: Replace with appropriate non-hardcoded url
        let url = &self.base_url;
        notice!("{}", url);
        if let Some(client) = &self.client {
            let mut offset: Option<String> = None;
            loop {
                // Fetch all of the rows upfront. Arguably, this could be done in batches (and invoked each
                // time iter_scan() runs out of rows) to pipeline the I/O, but we'd have to manage more
                // state so starting with the simpler solution.
                let url = self.set_limit_offset(&url, None, offset.as_deref())?;

                let body = self.rt.block_on(client.get(&url).send()).and_then(|resp| {
                    resp.error_for_status()
                        .and_then(|resp| self.rt.block_on(resp.text()))
                        .map_err(reqwest_middleware::Error::from)
                })?;
                notice!("{:?}", body);

                let (new_rows, new_offset) = self.parse_resp(&body, columns)?;
                notice!("finished parsing");
                rows.extend(new_rows);

                stats::inc_stats(Self::FDW_NAME, stats::Metric::BytesIn, body.len() as i64);

                if let Some(new_offset) = new_offset {
                    offset = Some(new_offset);
                } else {
                    break;
                }
            }
        }
        stats::inc_stats(Self::FDW_NAME, stats::Metric::RowsIn, rows.len() as i64);
        stats::inc_stats(Self::FDW_NAME, stats::Metric::RowsOut, rows.len() as i64);
        notice!("{:?}", rows);

        self.scan_result = Some(rows);
        Ok(())
    }

    fn iter_scan(&mut self, row: &mut Row) -> Auth0FdwResult<Option<()>> {
        // this is called on each row and we only return one row here
        notice!("called for row {:?}", row);

        if let Some(ref mut result) = self.scan_result {
            notice!("{:?}", result);
            if !result.is_empty() {
                return Ok(result
                    .drain(0..1)
                    .last()
                    .map(|src_row| row.replace_with(src_row)));
            }
        }
        // return 'None' to stop data scan
        Ok(None)
    }

    fn end_scan(&mut self) -> Auth0FdwResult<()> {
        notice!(" Before end scan");
        self.scan_result.take();
        // we do nothing here, but you can do things like resource cleanup and etc.
        Ok(())
    }
}

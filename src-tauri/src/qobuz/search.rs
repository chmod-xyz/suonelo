use super::{
    Credentials, Params, QobuzClient, QobuzError, f64_value, nonempty,
    resources::{identity, thumbnail_source},
    u32_value,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const PAGE_SIZE: usize = 20;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Album,
    Track,
    Artist,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Album, Self::Track, Self::Artist];

    pub fn group(self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Track => "tracks",
            Self::Artist => "artists",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Track => "track",
            Self::Artist => "artist",
        }
    }

    fn search_endpoint(self) -> &'static str {
        match self {
            Self::Album => "album/search",
            Self::Track => "track/search",
            Self::Artist => "artist/search",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct Id {
    pub kind: Kind,
    pub id: String,
}

impl Id {
    pub fn valid(&self) -> bool {
        match self.kind {
            Kind::Album => identity(&Value::String(self.id.clone())).as_ref() == Some(&self.id),
            Kind::Track | Kind::Artist => self
                .id
                .parse::<u64>()
                .is_ok_and(|id| id > 0 && id.to_string() == self.id),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Credit {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub key: Id,
    pub title: String,
    pub credits: Vec<Credit>,
    pub byline: Option<String>,
    pub edition: Option<String>,
    pub date: Option<String>,
    pub genre_id: Option<u32>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<f64>,
    pub hires: Option<bool>,
    pub explicit: Option<bool>,
    pub streamable: Option<bool>,
    pub track_count: Option<u32>,
    pub disc: Option<u32>,
    pub number: Option<u32>,
    pub duration: Option<u32>,
    #[serde(skip)]
    pub artwork: Option<String>,
    #[serde(skip)]
    pub primary_credit: Option<String>,
    #[serde(skip)]
    pub parent: Option<Arc<Row>>,
    pub release: Option<String>,
}

fn display(value: &Value) -> Option<String> {
    nonempty(value)
        .map(|s| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty())
}

fn credit(value: &Value) -> Option<Credit> {
    let id = identity(&value["id"])?;
    if !(Id {
        kind: Kind::Artist,
        id: id.clone(),
    })
    .valid()
    {
        return None;
    }
    Some(Credit {
        id,
        name: display(&value["name"])?,
    })
}

fn main_credit(value: &Value) -> bool {
    let roles = &value["roles"];
    let is_main = |role: &Value| {
        role.as_str().is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "main-artist" | "main artist"
            )
        })
    };
    is_main(roles)
        || roles
            .as_array()
            .is_some_and(|roles| roles.iter().any(is_main))
}

pub fn row(kind: Kind, value: &Value, parent: Option<Arc<Row>>) -> Option<Row> {
    if value["displayable"].as_bool() == Some(false) {
        return None;
    }
    let key = Id {
        kind,
        id: identity(&value["id"])?,
    };
    if !key.valid() {
        return None;
    }
    let primary = &value[match kind {
        Kind::Album => "artist",
        Kind::Track => "performer",
        Kind::Artist => "name",
    }];
    let primary_credit = credit(primary);
    let mut credits: Vec<_> = primary_credit.iter().cloned().collect();
    for item in value["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| main_credit(item))
    {
        if let Some(person) = credit(item)
            && !credits.iter().any(|current| current.id == person.id)
        {
            credits.push(person);
        }
    }
    let parent = if kind == Kind::Track {
        parent.or_else(|| row(Kind::Album, &value["album"], None).map(Arc::new))
    } else {
        None
    };
    let release = parent.as_ref().map(|row| row.key.id.clone()).or_else(|| {
        (kind == Kind::Track)
            .then(|| identity(&value["album"]["id"]))
            .flatten()
    });
    let date = display(&value["release_date_original"]).filter(|text| {
        let bytes = text.as_bytes();
        bytes.len() == 10
            && bytes.iter().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    *b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            })
    });
    Some(Row {
        key,
        title: display(
            &value[if kind == Kind::Artist {
                "name"
            } else {
                "title"
            }],
        )?,
        byline: display(&primary["name"]),
        credits,
        edition: display(&value["version"]),
        date,
        genre_id: u32_value(&value["genre"]["id"]),
        bit_depth: u32_value(&value["maximum_bit_depth"]),
        sample_rate: f64_value(&value["maximum_sampling_rate"])
            .filter(|rate| rate.is_finite() && *rate > 0.0),
        hires: value["hires"].as_bool(),
        explicit: value["parental_warning"].as_bool(),
        streamable: value["streamable"].as_bool(),
        track_count: u32_value(&value["tracks_count"]),
        disc: u32_value(&value["media_number"]),
        number: u32_value(&value["track_number"]),
        duration: u32_value(&value["duration"]),
        artwork: thumbnail_source(value)
            .filter(|source| {
                let Ok(url) = url::Url::parse(source) else {
                    return false;
                };
                url.host_str()
                    .is_some_and(|host| host == "qobuz.com" || host.ends_with(".qobuz.com"))
                    && !url.query_pairs().any(|(name, _)| {
                        matches!(
                            name.to_ascii_lowercase().as_str(),
                            "user_auth_token"
                                | "token"
                                | "access_token"
                                | "app_secret"
                                | "request_sig"
                        )
                    })
            })
            .or_else(|| parent.as_ref()?.artwork.clone()),
        primary_credit: primary_credit.map(|c| c.id),
        parent,
        release,
    })
}

pub struct Page {
    pub rows: Vec<Row>,
    pub offset: usize,
    pub length: usize,
    pub total: Option<usize>,
    pub ids: Vec<Option<String>>,
}

impl Page {
    pub fn read(
        container: &Value,
        kind: Kind,
        offset: usize,
        parent: Option<Arc<Row>>,
    ) -> Result<Self, QobuzError> {
        let items = container["items"]
            .as_array()
            .ok_or_else(|| QobuzError::invalid_response("Qobuz omitted a catalog results page."))?;
        let total = container["total"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());
        let reported_offset = container["offset"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());
        if reported_offset.is_some_and(|reported| reported != offset) && !items.is_empty() {
            return Err(QobuzError::invalid_response(
                "Qobuz returned a different catalog page than requested.",
            ));
        }
        Ok(Self {
            rows: items
                .iter()
                .filter_map(|item| row(kind, item, parent.clone()))
                .collect(),
            offset,
            length: items.len(),
            total,
            ids: items.iter().map(|item| identity(&item["id"])).collect(),
        })
    }
}

fn paging(offset: usize) -> Params {
    vec![
        ("limit", PAGE_SIZE.to_string()),
        ("offset", offset.to_string()),
    ]
}

impl QobuzClient {
    pub async fn search_catalog(
        &self,
        query: &str,
        account: &Credentials,
    ) -> Result<Vec<(Kind, Result<Page, QobuzError>)>, QobuzError> {
        let mut params = paging(0);
        params.push(("query", query.to_owned()));
        let response = self.metadata("catalog/search", params, account).await?;
        Ok(Kind::ALL
            .into_iter()
            .map(|kind| (kind, Page::read(&response[kind.group()], kind, 0, None)))
            .collect())
    }

    pub async fn search_page(
        &self,
        query: &str,
        kind: Kind,
        offset: usize,
        account: &Credentials,
    ) -> Result<Page, QobuzError> {
        let mut params = paging(offset);
        params.push(("query", query.to_owned()));
        let response = self
            .metadata(kind.search_endpoint(), params, account)
            .await?;
        Page::read(&response[kind.group()], kind, offset, None)
    }

    pub async fn search_detail(
        &self,
        key: &Id,
        offset: usize,
        account: &Credentials,
    ) -> Result<(Row, Option<Page>), QobuzError> {
        let mut params = paging(offset);
        let (endpoint, id_param, collection) = match key.kind {
            Kind::Album => ("album/get", "album_id", Some(Kind::Track)),
            Kind::Track => ("track/get", "track_id", None),
            Kind::Artist => {
                params.push(("extra", "albums".into()));
                ("artist/get", "artist_id", Some(Kind::Album))
            }
        };
        params.push((id_param, key.id.clone()));
        let response = self.metadata(endpoint, params, account).await?;
        let detail = row(key.kind, &response, None)
            .filter(|row| row.key == *key)
            .ok_or_else(|| {
                QobuzError::invalid_response("Qobuz did not return the requested catalog item.")
            })?;
        let page = collection
            .map(|kind| {
                let parent = (key.kind == Kind::Album).then(|| Arc::new(detail.clone()));
                Page::read(&response[kind.group()], kind, offset, parent)
            })
            .transpose()?;
        Ok((detail, page))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_rows_preserve_identity_collaborators_and_independent_flags() {
        let album = json!({
            "id":"000abc", "title":"Petal", "subtitle":"Ariana Grande", "version":null,
            "artist":{"id":713899,"name":"Ariana Grande"}, "artists":[
                {"id":17,"name":"Cynthia Erivo","roles":["main-artist"]},
                {"id":18,"name":"Producer","roles":["producer"]}
            ], "genre":{"id":117}, "parental_warning":true, "hires":true,
            "maximum_bit_depth":24, "maximum_sampling_rate":48, "release_date_original":"2026-01-01"
        });
        let track = json!({
            "id":9007199254740993_u64, "title":"Petal", "album":album,
            "parental_warning":false, "hires_streamable":true,
            "performer":{"id":17,"name":"Cynthia Erivo"}
        });
        let track = row(Kind::Track, &track, None).unwrap();
        let album = track.parent.unwrap();
        assert_eq!(track.key.id, "9007199254740993");
        assert_eq!(album.key.id, "000abc");
        assert_eq!(track.explicit, Some(false));
        assert_eq!(album.explicit, Some(true));
        assert_eq!(track.hires, None);
        assert_eq!(album.edition, None);
        assert_eq!(album.credits.len(), 2);
        assert_eq!(album.primary_credit.as_deref(), Some("713899"));
    }

    #[tokio::test]
    #[ignore = "Requires explicitly selected, authorized Qobuz portable vault and live connectivity"]
    async fn live_catalog_probe() {
        let vault_path =
            std::env::var_os("SUONELO_SEARCH_VAULT").expect("Choose the approved QA vault");
        let vault = crate::vault::Vault::new(vault_path.into());
        let account = vault
            .credentials()
            .expect("Could not open the QA vault with Windows data protection");
        let client = QobuzClient::new().unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(45), async {
            match client.request("user/get", Vec::new, account).await {
                Ok(body) => println!("account verified; numeric ID present={}", body["id"].as_u64().or_else(|| body["user"]["id"].as_u64()).is_some()),
                Err(failure) => println!("account probe returned {} ({:?})", failure.error.code, failure.status),
            }
            for (offset, limit, parent) in [(0, 100, None), (10, 10, None), (0, 100, Some(117)), (0, 100, Some(92)), (0, 100, Some(999999))] {
                let mut params = vec![("offset", offset.to_string()), ("limit", limit.to_string())];
                if let Some(parent) = parent { params.push(("parent_id", parent.to_string())); }
                match client.request("genre/list", || params.clone(), account).await {
                    Ok(value) => {
                        let genres = &value["genres"];
                        let rows = genres["items"].as_array().into_iter().flatten().map(|row| {
                            json!({"id":row["id"],"name":row["name"],"slug":row["slug"],"path":row["path"],"parent_id":row["parent_id"],"children":row["children"].as_array().map(Vec::len)})
                        }).collect::<Vec<_>>();
                        println!("taxonomy offset={offset} limit={limit} parent={parent:?} total={} items={}", genres["total"], json!(rows));
                    }
                    Err(failure) => println!("taxonomy request returned {} ({:?})", failure.error.code, failure.status),
                }
            }
            for query in ["petal ariana", "petal ari", "beyonce"] {
                let groups = client.search_catalog(query, account).await.map_err(|e| e.code).unwrap();
                for (kind, page) in groups {
                    let page = page.map_err(|e| e.code).unwrap();
                    println!("{query} {kind:?}: {}/ {:?}, first {:?}", page.length, page.total, page.rows.first().map(|row| (&row.key.id, &row.title)));
                }
            }
        }).await;
        assert!(result.is_ok(), "Live Qobuz catalog probe timed out");
    }

    #[tokio::test]
    #[ignore = "Read-only recursive taxonomy verification with an authorized Qobuz portable vault"]
    async fn capture_catalog_taxonomy() {
        let vault =
            crate::vault::Vault::new(std::env::var_os("SUONELO_SEARCH_VAULT").unwrap().into());
        let account = vault
            .credentials()
            .expect("Could not open the selected vault");
        let client = QobuzClient::new().unwrap();
        let mut pending = std::collections::VecDeque::<Option<u64>>::from([None]);
        let mut taxonomy = std::collections::BTreeMap::new();
        let mut requests = 0;
        while let Some(parent) = pending.pop_front() {
            let mut offset = 0;
            loop {
                requests += 1;
                assert!(requests <= 512, "Unexpected taxonomy size");
                let mut params = vec![("offset", offset.to_string()), ("limit", "20".to_string())];
                if let Some(id) = parent {
                    params.push(("parent_id", id.to_string()));
                }
                let response = tokio::time::timeout(
                    std::time::Duration::from_secs(12),
                    client.metadata("genre/list", params, account),
                )
                .await
                .expect("Taxonomy deadline exceeded")
                .unwrap_or_else(|err| panic!("taxonomy failed: {}", err.code));
                let group = &response["genres"];
                let items = group["items"].as_array().expect("Missing genre group");
                let total = group["total"].as_u64().expect("Missing genre total") as usize;
                for item in items {
                    let id = item["id"].as_u64().expect("Missing canonical genre ID");
                    let label = item["name"].as_str().expect("Missing localized label");
                    let slug = item["slug"].as_str().expect("Missing genre slug");
                    let path = item["path"]
                        .as_array()
                        .expect("Missing canonical path")
                        .iter()
                        .map(|x| x.as_u64().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(path.first().copied(), Some(id));
                    assert_eq!(path.get(1).copied(), parent);
                    assert!(taxonomy.insert(id, json!({"id":id,"label":label,"slug":slug,"parent":parent,"path":path})).is_none());
                    pending.push_back(Some(id));
                }
                offset += items.len();
                assert!(offset <= total);
                if offset == total {
                    break;
                }
                assert!(!items.is_empty(), "Non-advancing genre pagination");
            }
        }
        println!(
            "Verified {} catalog nodes over {requests} pages, {} roots",
            taxonomy.len(),
            taxonomy.values().filter(|x| x["parent"].is_null()).count()
        );
        for parent in [112, 91, 116, 123, 167, 0] {
            let params = vec![
                ("offset", "0".into()),
                ("limit", "1".into()),
                ("parent_id", parent.to_string()),
            ];
            match client
                .request("genre/list", || params.clone(), account)
                .await
            {
                Ok(v) => println!(
                    "additional genre root {parent}: total={}",
                    v["genres"]["total"]
                ),
                Err(err) => println!(
                    "additional genre root {parent}: {} ({:?})",
                    err.error.code, err.status
                ),
            }
        }
        let asset = json!({"apiVersion":"0.2","verifiedOn":"2026-11-07","genres":taxonomy.into_values().collect::<Vec<_>>()});
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(std::env::var_os("SUONELO_SEARCH_TAXONOMY_CAPTURE").unwrap())
            .unwrap();
        serde_json::to_writer_pretty(&mut file, &asset).unwrap();
    }

    #[tokio::test]
    #[ignore = "Read-only taxonomy scope comparison using authorized application access"]
    async fn taxonomy_scope_probe() {
        let vault =
            crate::vault::Vault::new(std::env::var_os("SUONELO_SEARCH_VAULT").unwrap().into());
        let account = vault
            .credentials()
            .expect("Could not open the selected vault");
        let client = QobuzClient::new().unwrap();
        for (name, params) in [
            ("baseline", vec![("limit", "100")]),
            ("country", vec![("limit", "100"), ("country", "GB")]),
            (
                "country_code",
                vec![("limit", "100"), ("country_code", "GB")],
            ),
            ("territory", vec![("limit", "100"), ("territory", "GB")]),
            ("lang", vec![("limit", "100"), ("lang", "en")]),
            ("pop rock", vec![("limit", "100"), ("parent_id", "112")]),
            ("reggae", vec![("limit", "100"), ("parent_id", "123")]),
        ] {
            for authenticate in [false, true] {
                let mut request = client
                    .api
                    .get(format!("{}genre/list", super::super::API))
                    .header("X-App-Id", &account.app_id)
                    .header("Accept-Language", "en-GB")
                    .header("Origin", super::super::PLAYER)
                    .query(&params);
                if authenticate {
                    request = request.query(&[("user_auth_token", &account.token)]);
                }
                match request.send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let body = response.json::<Value>().await.unwrap_or(Value::Null);
                        let genre_ids = body["genres"]["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|item| (&item["id"], &item["slug"], &item["path"]))
                            .collect::<Vec<_>>();
                        println!(
                            "{name} authenticated={authenticate} HTTP {status}: {}",
                            json!(genre_ids)
                        );
                    }
                    Err(_) => println!("{name} authenticated={authenticate} transport failed"),
                }
            }
        }
        for id in [112, 91, 116, 123, 167] {
            let params = vec![("genre_id", id.to_string())];
            match client
                .request("genre/get", || params.clone(), account)
                .await
            {
                Ok(row) => println!(
                    "genre/get {id}: name={} slug={} path={} keys={:?}",
                    row["name"],
                    row["slug"],
                    row["path"],
                    row.as_object().map(|obj| obj.keys().collect::<Vec<_>>())
                ),
                Err(err) => println!("genre/get {id}: {} ({:?})", err.error.code, err.status),
            }
        }
        for (field, slug) in [
            ("slug", "anime"),
            ("genre_id", "anime"),
            ("slug", "musique-de-films"),
            ("genre_id", "musique-de-films"),
        ] {
            let params = vec![(field, slug.to_owned())];
            match client
                .request("genre/get", || params.clone(), account)
                .await
            {
                Ok(row) => println!(
                    "genre/get by {field}={slug}: id={} slug={}",
                    row["id"], row["slug"]
                ),
                Err(err) => println!(
                    "genre/get by {field}={slug}: {} ({:?})",
                    err.error.code, err.status
                ),
            }
        }
    }

    #[tokio::test]
    #[ignore = "Resolves storefront category IDs omitted by the acceptance account's genre/list tree"]
    async fn resolve_storefront_genre_ids() {
        let vault =
            crate::vault::Vault::new(std::env::var_os("SUONELO_SEARCH_VAULT").unwrap().into());
        let account = vault
            .credentials()
            .expect("Could not open the selected vault");
        let client = QobuzClient::new().unwrap();
        let capture: Value = serde_json::from_reader(
            std::fs::File::open(std::env::var_os("SUONELO_SEARCH_TAXONOMY_CAPTURE").unwrap())
                .unwrap(),
        )
        .unwrap();
        let ids = capture["genres"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_u64().unwrap())
            .collect::<std::collections::HashSet<_>>();
        let mut found = Vec::new();
        let lower = *ids.iter().min().unwrap();
        let upper = *ids.iter().max().unwrap();
        assert!(
            upper - lower < 256,
            "Review a larger canonical ID range before probing"
        );
        let range = if std::env::var_os("SUONELO_SEARCH_GENRE_NEW_IDS").is_some() {
            upper + 1..=upper + 20
        } else {
            lower..=upper
        };
        for id in range.filter(|id| !ids.contains(id)) {
            let params = vec![("genre_id", id.to_string())];
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                client.request("genre/get", || params.clone(), account),
            )
            .await
            .expect("Genre identity request exceeded deadline");
            match response {
                Ok(row) => {
                    assert_eq!(row["id"].as_u64(), Some(id));
                    found.push(json!({"id":row["id"],"label":row["name"],"slug":row["slug"],"path":row["path"]}));
                }
                Err(error) if error.status == Some(reqwest::StatusCode::NOT_FOUND) => (),
                Err(error) => panic!(
                    "Genre lookup failed for {id}: {} {:?}",
                    error.error.code, error.status
                ),
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(std::env::var_os("SUONELO_SEARCH_GENRE_IDENTITIES").unwrap())
            .unwrap();
        println!("Verified {} further genre identities", found.len());
        serde_json::to_writer_pretty(&mut file, &found).unwrap();
    }
}

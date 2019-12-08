use semver::VersionReq;
use semver::Version as SemVer;
use std::sync::Arc;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use serde_derive::*;
use std::sync::mpsc;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

fn crate_url(name: &str) -> String {
    const BASE_URL: &str = "https://lib.rs/registry-proxy/";
    let mut url = String::with_capacity(200);

    url.push_str(BASE_URL);
    match name.len() {
        1 => {
            url.push_str("1/");
            url.push_str(name);
        },
        2 => {
            url.push_str("2/");
            url.push_str(name);
        },
        3 => {
            url.push_str("3/");
            url.push(name.chars().nth(0).unwrap());
            url.push('/');
            url.push_str(name);
        },
        _ => {
            let mut c = name.chars();
            url.push(c.next().unwrap());
            url.push(c.next().unwrap());
            url.push('/');
            url.push(c.next().unwrap());
            url.push(c.next().unwrap());
            url.push('/');
            url.push_str(name);
        },
    }
    url.to_lowercase()
}

#[derive(Debug, Deserialize)]
struct Crate {
    name: String,
    vers: String,
    deps: Vec<Dep>,
    cksum: String,
    features: HashMap<String, Vec<String>>,
    yanked: bool
}

#[derive(Debug, Deserialize)]
struct Dep {
    name: String,
    package: Option<String>,
    req: String,
    features: Vec<String>,
    optional: bool,
    default_features: bool,
    target: Option<String>,
    kind: Option<String>,
}

struct Exploration {
    client: Arc<reqwest::Client>,
    crates: HashMap<String, Option<Arc<Vec<Crate>>>>,
    done: HashMap<String, LookupFeatures>,
    todo: HashMap<String, LookupFeatures>,
    sender: Arc<mpsc::SyncSender<Result<(String, Vec<Crate>), BoxErr>>>,
    receiver: mpsc::Receiver<Result<(String, Vec<Crate>), BoxErr>>,
}

#[derive(Debug, Clone)]
struct LookupFeatures {
    features: HashSet<String>,
    reqs: HashSet<VersionReq>,
}

impl LookupFeatures {
    pub fn merge(&mut self, other: &LookupFeatures) -> bool {
        let mut changed = false;
        for f in &other.features {
            if self.features.get(f).is_none() {
                self.features.insert(f.to_owned());
                changed = true;
            }
        }
        for r in &other.reqs {
            if self.reqs.get(r).is_none() {
                self.reqs.insert(r.to_owned());
                changed = true;
            }
        }
        changed
    }
}

impl Exploration {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(99);
        Self {
            client: Arc::new(reqwest::Client::builder()
                .gzip(true)
                .use_rustls_tls()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(20))
                .build().unwrap()),
            sender: Arc::new(sender),
            receiver,
            crates: HashMap::new(),
            done: HashMap::new(),
            todo: HashMap::new(),
        }
    }

    fn fetch(client: &reqwest::Client, name: &str) -> Result<Vec<Crate>, BoxErr> {
        let url = crate_url(&name);
        eprintln!("Fetching {} from {}", name, url);
        let mut res = client.get(&url).send()?.error_for_status()?;
        let body = res.text()?;
        let crate_versions = body.lines().map(|l| {
            serde_json::from_str(l).map_err(|e| {
                format!("{}; {} bodylen={}, while parsing {} from {}", e, url, body.len(), name, l)
            })
        }).collect::<Result<Vec<Crate>, _>>()?;
        Ok(crate_versions)
    }

    pub fn enqueue(&mut self, name: String, wants: LookupFeatures) -> Result<(), BoxErr> {
        match self.done.entry(name.to_string()) {
            Entry::Vacant(e) => {
                e.insert(wants.clone());
            },
            Entry::Occupied(mut e) => {
                let f = e.get_mut();
                if !f.merge(&wants) {
                    // Nothing new required
                    return Ok(());
                }
            }
        };

        match self.crates.get(&name).cloned() {
            // If possible to process immediately, do it without enqueueing
            Some(Some(versions)) => {
                let wants = match self.todo.remove(&name) {
                    Some(mut k) => {
                        k.merge(&wants);
                        k
                    },
                    None => wants,
                };
                return self.process(&wants, &versions);
            },
            // Already fetching
            Some(None) => {}
            // Start a fetch
            None => {
                self.crates.insert(name.clone(), None);
                let name = name.clone();
                let tx = self.sender.clone();
                let client = self.client.clone();
                std::thread::spawn(move || {
                    tx.send(Self::fetch(&client, &name).map(|f| (name, f))).expect("send");
                });
            },
        }

        match self.todo.entry(name) {
            Entry::Vacant(e) => {
                e.insert(wants);
            },
            Entry::Occupied(mut e) => {
                e.get_mut().merge(&wants);
            },
        }
        Ok(())
    }

    pub fn process_all(&mut self) -> Result<(), BoxErr> {
        loop {
            if self.todo.is_empty() {
                return Ok(());
            }
            let (name, crates) = self.receiver.recv()??;
            let todo = self.todo.remove(&name);
            let crates = Arc::new(crates);
            self.crates.insert(name, Some(crates.clone()));
            if let Some(wants) = todo {
                self.process(&wants, &crates)?;
            }
        }
    }

    pub fn process(&mut self, wants: &LookupFeatures, versions: &[Crate]) -> Result<(), BoxErr> {
        let mut enabled_optional_crates = HashMap::new();
        for f in &wants.features {
            let mut s = f.splitn(2, '/');
            let name = s.next().unwrap();
            let f = enabled_optional_crates.entry(name.to_owned()).or_insert_with(Vec::new);
            if let Some(with_feature) = s.next() {
                if !with_feature.is_empty() {
                    f.push(with_feature.to_string());
                }
            }
        }

        let mut dep_features = HashMap::new();
        for v in versions {
            let semver = SemVer::parse(&v.vers).map_err(|e| format!("semver of {}@{}: {}", v.name, v.vers, e))?;
            if !wants.reqs.iter().any(|r| r.matches(&semver)) {
                continue;
            }
            for d in &v.deps {
                if d.kind.as_ref().map_or(false, |k| k == "dev") {
                    continue;
                }
                let package = d.package.as_ref().unwrap_or(&d.name);
                // do features refer to renamed packages?
                if d.optional && enabled_optional_crates.get(&d.name).is_none() && enabled_optional_crates.get(package).is_none() {
                    continue;
                }

                let req = VersionReq::parse(&d.req).map_err(|e| format!("dep {} of {}: {}", d.name, v.name, e))?;
                let dep_f_r = dep_features.entry(package.to_string())
                    .or_insert_with(|| (HashSet::new(), HashSet::new()));
                dep_f_r.1.insert(req);
                if d.default_features {
                    dep_f_r.0.insert("default".to_owned());
                }
                dep_f_r.0.extend(d.features.iter().filter(|f| !f.is_empty()).cloned());
            }
        }

        for (dep, (features, reqs)) in dep_features {
            self.enqueue(dep, LookupFeatures {
                features,
                reqs,
            })?;
        }
        Ok(())
    }
}

fn main() -> Result<(), BoxErr> {
    let mut e = Exploration::new();

    let start = std::time::Instant::now();
    for arg in std::env::args().skip(1) {
        let mut parts = arg.splitn(2, '@');
        let name = parts.next().unwrap();
        let ver = parts.next();

        let mut features = HashSet::new();
        features.insert("default".to_string());
        let mut reqs = HashSet::new();
        reqs.insert(VersionReq::parse(ver.unwrap_or("*")).unwrap());
        e.enqueue(name.to_string(), LookupFeatures {features, reqs})?;
    }
    e.process_all()?;
    let elapsed = start.elapsed().as_millis();

    if e.done.is_empty() {
        eprintln!("Specify names of crates as arguments using name@version format, e.g. actix-web@1.0");
    } else {
        println!("Discovered {} crates in {}ms: {}", e.done.len(), elapsed, e.done.keys().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

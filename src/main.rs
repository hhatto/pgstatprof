extern crate getopts;
#[macro_use]
extern crate lazy_static;
extern crate postgres;
extern crate regex;
extern crate time;
extern crate users;

use std::borrow::Cow;
use std::collections::HashMap;
use std::{env, process, thread};
use std::time::Duration;
use postgres::{Connection, TlsMode};
use getopts::Options;
use users::{get_current_uid, get_user_by_uid};
use time::{now, strftime};
use regex::Regex;

const QUERY_SHOW_PROCESS: &'static str = "SELECT state,query FROM pg_stat_activity";

lazy_static! {
    static ref NORMALIZE_PATTERNS: Vec<NormalizePattern<'static>> = vec![
        NormalizePattern::new(Regex::new(r" +").expect("fail regex compile: +"), " "),
        NormalizePattern::new(Regex::new(r#"[+-]{0,1}\b\d+\b"#).expect("fail regex compile: digit"), "N"),
        NormalizePattern::new(Regex::new(r"\b0x[0-9A-Fa-f]+\b").expect("fail regex compile: hex"), "0xN"),
        NormalizePattern::new(Regex::new(r#"(\\')"#).expect("fail regex compile: single quote"), ""),
        NormalizePattern::new(Regex::new(r#"(\\")"#).expect("fail regex compile: double quote"), ""),
        NormalizePattern::new(Regex::new(r"'[^']+'").expect("fail regex compile: string1"), "S"),
        NormalizePattern::new(Regex::new(r#""[^"]+""#).expect("fail regex compile: string2"), "S"),
        NormalizePattern::new(Regex::new(r"(([NS]\s*,\s*){4,})").expect("fail regex compile: long"), "...")
    ];
}

trait Summarize {
    fn new(limit: u32) -> Self;
    fn show(&mut self, n_query: u32);
    fn update(&mut self, queries: Vec<String>) -> usize;
}

fn show_summary(summ: &HashMap<String, i64>, n_query: u32) {
    let mut pp: Vec<_> = summ.iter().collect();
    pp.sort_by(|a, b| b.1.cmp(a.1));

    let mut cnt = 0;
    for (k, v) in pp {
        println!("{:-4} {}", v, k);
        cnt += 1;
        if cnt >= n_query {
            break;
        }
    }
}

struct Summarizer {
    counts: HashMap<String, i64>,
}
impl Summarize for Summarizer {
    fn new(_: u32) -> Summarizer {
        Summarizer {
            counts: HashMap::new(),
        }
    }

    fn show(&mut self, n_query: u32) {
        show_summary(&self.counts, n_query);
    }

    fn update(&mut self, queries: Vec<String>) -> usize {
        for query in queries {
            let count = self.counts.entry(query).or_insert(0);
            *count += 1;
        }

        self.counts.len()
    }
}

#[derive(Debug)]
struct QueryCount {
    q: String,
    n: i64,
}
struct RecentSummarizer {
    counts: Vec<Vec<QueryCount>>,
    limit: u32,
}
impl Summarize for RecentSummarizer {
    fn new(limit: u32) -> RecentSummarizer {
        RecentSummarizer {
            counts: vec![],
            limit: limit,
        }
    }

    fn show(&mut self, n_query: u32) {
        let mut summ = HashMap::new();
        for qcs in &self.counts {
            for qc in qcs {
                let query = qc.q.clone();
                let count = summ.entry(query).or_insert(0);
                *count += qc.n;
            }
        }
        show_summary(&summ, n_query);
    }

    fn update(&mut self, queries: Vec<String>) -> usize {
        let mut qs = queries;
        let mut qc = Vec::<QueryCount>::new();
        if self.counts.len() >= self.limit as usize {
            self.counts.remove(0);
        }
        qs.sort_by(|a, b| a.cmp(b));

        let mut last_query = "";
        for query in qs.iter() {
            if last_query != query.as_str() {
                qc.push(QueryCount {
                    q: query.clone(),
                    n: 0,
                });
                last_query = query.as_str();
            }
            let l = qc.last_mut().expect("fail get last query string");
            l.n += 1;
        }
        self.counts.push(qc);

        self.counts.len()
    }
}

#[derive(Debug)]
struct Process {
    state: String,
    info: String,
}

struct NormalizePattern<'a> {
    re: Regex,
    subs: &'a str,
}

impl<'a> NormalizePattern<'a> {
    fn new(re: Regex, subs: &'a str) -> NormalizePattern<'a> {
        NormalizePattern { re: re, subs: subs }
    }
    fn normalize(&self, text: &'a str) -> Cow<'a, str> {
        self.re.replace_all(text, self.subs)
    }
}

struct ProfilerOption {
    interval: f32,
    delay: i32,
    top: u32,
    diff: bool,
    normalize: bool,
}

macro_rules! opts2v {
    ($m:expr, $opts:expr, $opt:expr, $t:ty, $default:expr) => (
        match $m.opt_str($opt) {
            Some(v) => {
                match v.parse::<$t>() {
                    Ok(v) => v,
                    Err(e) => {
                        println!("e={:?}", e);
                        print_usage($opts);
                        process::exit(1);
                    },
                }
            },
            None => $default,
        }
        )
}

pub fn normalize_query(text: &str) -> String {
    let mut t = text.to_string();
    for pat in NORMALIZE_PATTERNS.iter() {
        t = pat.normalize(t.as_str()).into();
    }
    t.to_string()
}

fn get_process_list(conn: &Connection) -> Vec<Process> {
    let mut procs: Vec<Process> = vec![];
    for row in &conn.query(QUERY_SHOW_PROCESS, &[]).expect("fail query()") {
        let state = match row.get_opt(0) {
            Some(Ok(d)) => d,
            _ => "".to_string(),
        };
        let info = match row.get_opt(1) {
            Some(Ok(d)) => d,
            _ => "".to_string(),
        };
        if state != "active" || info == "" || info == QUERY_SHOW_PROCESS {
            continue;
        }

        procs.push(Process {
            state: state,
            info: info,
        });
    }
    procs
}

fn print_usage(opts: Options) {
    print!("{}", opts.usage("Usage: pgstatprof [options]"));
}

fn exec_profile<T: Summarize>(conn: &Connection, mut summ: T, options: &ProfilerOption) {
    let mut cnt = 0;
    let mut old_summary_cnt = 0;
    loop {
        let mut procs = get_process_list(conn);
        for process in procs.iter_mut() {
            if options.normalize {
                let info = normalize_query(process.info.as_str());
                (*process).info = info;
            }
        }

        let summary_cnt = summ.update(procs.iter().map(|x| x.info.clone()).collect());

        cnt += 1;
        if cnt >= options.delay {
            cnt = 0;

            let is_print = if !options.diff {
                true
            } else if old_summary_cnt != summary_cnt {
                true
            } else {
                false
            };

            if is_print {
                let t = now().to_local();
                println!(
                    "##  {}.{:03} {}",
                    strftime("%Y-%m-%d %H:%M:%S", &t).expect("fail strftime(ymdhms)"),
                    t.tm_nsec / 1000_000,
                    strftime("%z", &t).expect("fail strftime(z)")
                );

                summ.show(options.top);

                old_summary_cnt = summary_cnt;
            }
        }

        thread::sleep(Duration::from_millis((1000. * options.interval) as u64));
    }
}

fn main() {
    let mut opts = Options::new();
    opts.optopt("h", "host", "postgresql hostname", "HOSTNAME");
    opts.optopt("u", "user", "postgresql user", "USER");
    opts.optopt("p", "password", "postgresql password", "PASSWORD");
    opts.optopt("", "port", "postgresql port", "PORT");
    opts.optopt("", "database", "database name", "DATABASENAME");
    opts.optopt("", "top", "print top N query (default: 10)", "N");
    opts.optopt(
        "",
        "last",
        "last N samples are summarized. 0 means summarize all samples",
        "N",
    );
    opts.optopt("i", "interval", "(float) Sampling interval", "N.M");
    opts.optflag("", "diff", "only output when existing new query (default: false)");
    opts.optflag("", "no-normalize", "normalize queries (default: false)");
    opts.optopt(
        "",
        "delay",
        "(int) Show summary for each `delay` samples. -interval=0.1 -delay=30 shows summary for every 3sec",
        "N",
    );
    let args: Vec<String> = env::args().collect();
    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(e) => {
            print_usage(opts);
            println!("{:?}", e);
            return;
        }
    };

    let host = match matches.opt_str("host") {
        Some(v) => v,
        None => "localhost".to_string(),
    };
    let user = match matches.opt_str("user") {
        Some(v) => v,
        None => get_user_by_uid(get_current_uid())
            .expect("fail get uid")
            .name()
            .to_string(),
    };
    let password = match matches.opt_str("password") {
        Some(v) => v,
        None => "".to_string(),
    };
    let database = match matches.opt_str("database") {
        Some(v) => v,
        None => "".to_string(),
    };

    let port = opts2v!(matches, opts, "port", i32, 5432);
    let last = opts2v!(matches, opts, "last", u32, 0);
    let options = ProfilerOption {
        interval: opts2v!(matches, opts, "interval", f32, 1.0),
        delay: opts2v!(matches, opts, "delay", i32, 1),
        top: opts2v!(matches, opts, "top", u32, 10),
        diff: !matches.opt_present("diff"),
        normalize: !matches.opt_present("no-normalize"),
    };

    let conn_uri = format!(
        "postgres://{user}:{password}@{host}:{port}/{database}",
        user = user,
        password = password,
        host = host,
        port = port,
        database = database
    );
    let conn = Connection::connect(conn_uri.as_str(), TlsMode::None)
        .expect(format!("fail get postgres connection: {}", conn_uri).as_str());

    if last == 0 {
        let summ: Summarizer = Summarize::new(last);
        exec_profile(&conn, summ, &options);
    } else {
        let summ: RecentSummarizer = Summarize::new(last);
        exec_profile(&conn, summ, &options);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize() {
        let data = vec![
            ("IN ('a', 'b', 'c')", "IN (S, S, S)"),
            ("IN ('a', 'b', 'c', 'd', 'e')", "IN (...S)"),
            ("IN (1, 2, 3)", "IN (N, N, N)"),
            ("IN (0x1, 2, 3)", "IN (0xN, N, N)"),
            ("IN (1, 2, 3, 4, 5)", "IN (...N)"),
        ];
        for (pat, ret) in data {
            println!("vv | {:?}, {:?}", normalize_query(pat), ret);
            assert!(normalize_query(pat) == ret);
        }
    }
}

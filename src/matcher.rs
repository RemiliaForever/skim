use event::{Event, EventReceiver, EventSender};
use item::{Item, ItemGroup, MatchedItem, MatchedItemGroup, MatchedRange};
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};
use std::thread;

use options::SkimOptions;
use regex::Regex;
use score;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

lazy_static! {
    static ref RANK_CRITERION: RwLock<Vec<RankCriteria>> = RwLock::new(vec![
        RankCriteria::Score,
        RankCriteria::Index,
        RankCriteria::Begin,
        RankCriteria::End,
    ]);
    static ref RE_AND: Regex = Regex::new(r"([^ |]+( +\| +[^ |]*)+)|( +)").unwrap();
    static ref RE_OR: Regex = Regex::new(r" +\| +").unwrap();
}

#[derive(Clone, Copy, Debug)]
enum Algorithm {
    PrefixExact,
    SuffixExact,
    Exact,
    InverseExact,
    InverseSuffixExact,
}

#[derive(Clone, Copy, PartialEq)]
enum MatcherMode {
    Regex,
    Fuzzy,
    Exact,
}

pub struct Matcher {
    tx_result: EventSender,
    mode: MatcherMode,
}

impl Matcher {
    pub fn new(tx_result: EventSender) -> Self {
        Matcher {
            tx_result,
            mode: MatcherMode::Fuzzy,
        }
    }

    pub fn parse_options(&mut self, options: &SkimOptions) {
        if let Some(ref tie_breaker) = options.tiebreak {
            let mut vec = Vec::new();
            for criteria in tie_breaker.split(',') {
                if let Some(c) = parse_criteria(criteria) {
                    vec.push(c);
                }
            }
            *RANK_CRITERION
                .write()
                .expect("matcher:parse_options: failed to lock RANK_CRITERION") = vec;
        }

        if options.tac {
            let mut ranks = RANK_CRITERION
                .write()
                .expect("matcher:parse_options: failed to lock RANK_CRITERION");
            for rank in ranks.iter_mut() {
                match *rank {
                    RankCriteria::Index => *rank = RankCriteria::NegIndex,
                    RankCriteria::NegIndex => *rank = RankCriteria::Index,
                    _ => {}
                }
            }
        }

        if options.exact {
            self.mode = MatcherMode::Exact;
        }

        if options.regex {
            self.mode = MatcherMode::Regex;
        }
    }

    pub fn run(&self, rx_item: EventReceiver) {
        let (tx_matcher, rx_matcher): (EventSender, EventReceiver) = channel();
        let matcher_restart = Arc::new(AtomicBool::new(false));
        // start a new thread listening for EvMatcherRestart, that means the query had been
        // changed, so that matcher shoudl discard all previous events.
        {
            let matcher_restart = Arc::clone(&matcher_restart);
            thread::spawn(move || {
                while let Ok((ev, arg)) = rx_item.recv() {
                    debug!("matcher: rx_item: {:?}", ev);
                    match ev {
                        Event::EvMatcherRestart => {
                            matcher_restart.store(true, Ordering::Relaxed);

                            let _ = tx_matcher.send((ev, Box::new(true)));
                            while matcher_restart.load(Ordering::Relaxed) {
                                thread::sleep(Duration::from_millis(10));
                            }

                            let _ = tx_matcher.send((ev, arg));
                        }

                        Event::EvActAccept | Event::EvActAbort => {
                            // quit the loop
                            break;
                        }

                        _ => {
                            // pass through all other events
                            let _ = tx_matcher.send((ev, arg));
                        }
                    }
                }
            });
        }

        let mut matcher_engine: Option<Box<MatchEngine>> = None;
        let mut num_processed: usize = 0;
        let mut matcher_mode = self.mode;

        while let Ok((ev, arg)) = rx_matcher.recv() {
            debug!("matcher: rx_matcher: {:?}", ev);
            if matcher_restart.load(Ordering::Relaxed) {
                while let Ok(_) = rx_matcher.try_recv() {}
                matcher_restart.store(false, Ordering::Relaxed);
                continue;
            }

            match ev {
                Event::EvMatcherRestart => {
                    num_processed = 0;
                    let query = arg.downcast::<String>()
                        .expect("matcher:EvMatcherRestart: failed to get arguments");

                    // notifiy the model that the query had been changed
                    let _ = self.tx_result.send((Event::EvModelRestart, Box::new(true)));

                    let mode_string = match matcher_mode {
                        MatcherMode::Regex => "RE".to_string(),
                        MatcherMode::Exact => "EX".to_string(),
                        _ => "".to_string(),
                    };
                    let _ = self.tx_result
                        .send((Event::EvModelNotifyMatcherMode, Box::new(mode_string)));

                    matcher_engine = Some(EngineFactory::build(&query, matcher_mode));
                }

                Event::EvMatcherNewItem => {
                    let items: ItemGroup = *arg.downcast()
                        .expect("matcher:EvMatcherNewItem: failed to get arguments");
                    num_processed += items.len();

                    if let Some(mat) = matcher_engine.as_ref() {
                        let matched_items: MatchedItemGroup =
                            items.into_iter().filter_map(|item| mat.match_item(item)).collect();
                        let _ = self.tx_result.send((Event::EvModelNewItem, Box::new(matched_items)));
                    }

                    // report the number of processed items
                    let _ = self.tx_result
                        .send((Event::EvModelNotifyProcessed, Box::new(num_processed)));
                }

                Event::EvReaderStopped | Event::EvReaderStarted => {
                    let _ = self.tx_result.send((ev, arg));
                }
                Event::EvSenderStopped => {
                    // Since matcher is single threaded, sender stopped means all items are
                    // processed.
                    let _ = self.tx_result
                        .send((Event::EvModelNotifyProcessed, Box::new(num_processed)));
                    let _ = self.tx_result.send((Event::EvMatcherStopped, arg));
                }

                Event::EvActRotateMode => {
                    if self.mode == MatcherMode::Regex {
                        // sk started with regex mode.
                        matcher_mode = if matcher_mode == self.mode {
                            MatcherMode::Fuzzy
                        } else {
                            MatcherMode::Regex
                        };
                    } else {
                        matcher_mode = if matcher_mode == self.mode {
                            MatcherMode::Regex
                        } else {
                            self.mode
                        }
                    }
                }

                _ => {}
            }
        }
    }
}

// A match engine will execute the matching algorithm
trait MatchEngine {
    fn match_item(&self, item: Arc<Item>) -> Option<MatchedItem>;
    fn display(&self) -> String;
}

fn build_rank(score: i64, index: i64, begin: i64, end: i64) -> [i64; 4] {
    let mut rank = [0; 4];
    for (idx, criteria) in (*RANK_CRITERION
        .read()
        .expect("matcher:build_rank: failed to lock RANK_CRITERION"))
        .iter()
        .enumerate()
        .take(4)
    {
        rank[idx] = match *criteria {
            RankCriteria::Score => score,
            RankCriteria::Index => index,
            RankCriteria::Begin => begin,
            RankCriteria::End => end,
            RankCriteria::NegScore => -score,
            RankCriteria::NegIndex => -index,
            RankCriteria::NegBegin => -begin,
            RankCriteria::NegEnd => -end,
        }
    }
    rank
}

//------------------------------------------------------------------------------
// Regular Expression engine
#[derive(Debug)]
struct RegexEngine {
    query_regex: Option<Regex>,
}

impl RegexEngine {
    pub fn builder(query: &str) -> Self {
        RegexEngine {
            query_regex: Regex::new(query).ok(),
        }
    }

    pub fn build(self) -> Self {
        self
    }
}

impl MatchEngine for RegexEngine {
    fn match_item(&self, item: Arc<Item>) -> Option<MatchedItem> {
        let mut matched_result = None;
        for &(start, end) in item.get_matching_ranges() {
            if self.query_regex.is_none() {
                matched_result = Some((0, 0));
                break;
            }

            let source: String = item.get_chars()[start..end].iter().cloned().collect();
            matched_result = score::regex_match(&source, &self.query_regex).map(|(s, e)| (s + start, e + start));

            if matched_result.is_some() {
                break;
            }
        }

        let (begin, end) = matched_result?;
        let score = (end - begin) as i64;
        let rank = build_rank(-score, item.get_index() as i64, begin as i64, end as i64);

        Some(
            MatchedItem::builder(item)
                .rank(rank)
                .matched_range(MatchedRange::Range(begin, end))
                .build(),
        )
    }

    fn display(&self) -> String {
        format!(
            "(Regex: {})",
            self.query_regex
                .as_ref()
                .map_or("".to_string(), |re| re.as_str().to_string())
        )
    }
}

//------------------------------------------------------------------------------
// Fuzzy engine
#[derive(Debug)]
struct FuzzyEngine {
    query: String,
    query_chars: Vec<char>,
}

impl FuzzyEngine {
    pub fn builder(query: &str) -> Self {
        FuzzyEngine {
            query: query.to_string(),
            query_chars: query.chars().collect(),
        }
    }

    pub fn build(self) -> Self {
        self
    }
}

impl MatchEngine for FuzzyEngine {
    fn match_item(&self, item: Arc<Item>) -> Option<MatchedItem> {
        // iterate over all matching fields:
        let mut matched_result = None;
        for &(start, end) in item.get_matching_ranges() {
            let source = &item.get_chars()[start..end];

            matched_result = score::fuzzy_match(source, &self.query_chars).map(|(s, vec)| {
                if start != 0 {
                    (s, vec.iter().map(|x| x + start).collect())
                } else {
                    (s, vec)
                }
            });

            if matched_result.is_some() {
                break;
            }
        }

        if matched_result == None {
            return None;
        }

        let (score, matched_range) = matched_result.unwrap();

        let begin = *matched_range.get(0).unwrap_or(&0) as i64;
        let end = *matched_range.last().unwrap_or(&0) as i64;

        let rank = build_rank(-score, item.get_index() as i64, begin, end);

        Some(
            MatchedItem::builder(item)
                .rank(rank)
                .matched_range(MatchedRange::Chars(matched_range))
                .build(),
        )
    }

    fn display(&self) -> String {
        format!("(Fuzzy: {})", self.query)
    }
}

//------------------------------------------------------------------------------
// Exact engine
#[derive(Debug)]
struct ExactEngine {
    query: String,
    query_chars: Vec<char>,
    algorithm: Algorithm,
}

impl ExactEngine {
    pub fn builder(query: &str, algo: Algorithm) -> Self {
        ExactEngine {
            query: query.to_string(),
            query_chars: query.chars().collect(),
            algorithm: algo,
        }
    }

    pub fn build(self) -> Self {
        self
    }

    fn match_item_exact(&self, item: Arc<Item>, filter: ExactFilter) -> Option<MatchedItem> {
        let mut matched_result = None;
        let mut range_start = 0;
        let mut range_end = 0;
        for &(start, end) in item.get_matching_ranges() {
            if self.query == "" {
                matched_result = Some(((0, 0), (0, 0)));
                break;
            }

            let chars: Vec<_> = item.get_text().chars().collect();
            let source: String = chars[start..end].iter().cloned().collect();
            matched_result = score::exact_match(&source, &self.query);

            if matched_result.is_some() {
                range_start = start;
                range_end = end;
                break;
            }
        }

        let (s, e) = filter(&matched_result, range_end - range_start)?;

        let (begin, end) = (s + range_start, e + range_start);
        let score = (end - begin) as i64;
        let rank = build_rank(-score, item.get_index() as i64, begin as i64, end as i64);

        Some(
            MatchedItem::builder(item)
                .rank(rank)
                .matched_range(MatchedRange::Range(begin, end))
                .build(),
        )
    }
}

// <Option<(start, end), (start, end)>, item_length> -> Option<(start, end)>
type ExactFilter = Box<Fn(&Option<((usize, usize), (usize, usize))>, usize) -> Option<(usize, usize)>>;

impl MatchEngine for ExactEngine {
    fn match_item(&self, item: Arc<Item>) -> Option<MatchedItem> {
        match self.algorithm {
            Algorithm::Exact => self.match_item_exact(
                item,
                Box::new(|matched_result, _| matched_result.map(|(first, _)| first)),
            ),
            Algorithm::InverseExact => self.match_item_exact(
                item,
                Box::new(
                    |matched_result, _| {
                        if matched_result.is_none() {
                            Some((0, 0))
                        } else {
                            None
                        }
                    },
                ),
            ),
            Algorithm::PrefixExact => self.match_item_exact(
                item,
                Box::new(|matched_result, _| match *matched_result {
                    Some(((s, e), _)) if s == 0 => Some((s, e)),
                    _ => None,
                }),
            ),
            Algorithm::SuffixExact => self.match_item_exact(
                item,
                Box::new(|matched_result, len| match *matched_result {
                    Some((_, (s, e))) if e == len => Some((s, e)),
                    _ => None,
                }),
            ),
            Algorithm::InverseSuffixExact => self.match_item_exact(
                item,
                Box::new(|matched_result, len| match *matched_result {
                    Some((_, (_, e))) if e != len => None,
                    _ => Some((0, 0)),
                }),
            ),
        }
    }

    fn display(&self) -> String {
        format!("({:?}: {})", self.algorithm, self.query)
    }
}

//------------------------------------------------------------------------------
// OrEngine, a combinator
struct OrEngine {
    engines: Vec<Box<MatchEngine>>,
}

impl OrEngine {
    pub fn builder(query: &str, mode: MatcherMode) -> Self {
        // mock
        OrEngine {
            engines: RE_OR.split(query).map(|q| EngineFactory::build(q, mode)).collect(),
        }
    }

    pub fn build(self) -> Self {
        self
    }
}

impl MatchEngine for OrEngine {
    fn match_item(&self, item: Arc<Item>) -> Option<MatchedItem> {
        for engine in &self.engines {
            let result = engine.match_item(Arc::clone(&item));
            if result.is_some() {
                return result;
            }
        }

        None
    }

    fn display(&self) -> String {
        format!(
            "(Or: {})",
            self.engines.iter().map(|e| e.display()).collect::<Vec<_>>().join(", ")
        )
    }
}

//------------------------------------------------------------------------------
// AndEngine, a combinator
struct AndEngine {
    engines: Vec<Box<MatchEngine>>,
}

impl AndEngine {
    pub fn builder(query: &str, mode: MatcherMode) -> Self {
        let query_trim = query.trim_matches(|c| c == ' ' || c == '|');
        let mut engines = vec![];
        let mut last = 0;
        for mat in RE_AND.find_iter(query_trim) {
            let (start, end) = (mat.start(), mat.end());
            let term = &query_trim[last..start].trim_matches(|c| c == ' ' || c == '|');
            if !term.is_empty() {
                engines.push(EngineFactory::build(term, mode));
            }

            if !mat.as_str().trim().is_empty() {
                engines.push(Box::new(OrEngine::builder(mat.as_str().trim(), mode).build()));
            }
            last = end;
        }

        let term = &query_trim[last..].trim_matches(|c| c == ' ' || c == '|');
        if !term.is_empty() {
            engines.push(EngineFactory::build(term, mode));
        }

        AndEngine { engines }
    }

    pub fn build(self) -> Self {
        self
    }

    fn merge_matched_items(&self, mut items: Vec<MatchedItem>) -> MatchedItem {
        items.sort();
        let rank = items[0].rank;
        let item = Arc::clone(&items[0].item);
        let mut ranges = vec![];
        for item in items {
            match item.matched_range {
                Some(MatchedRange::Range(start, end)) => {
                    ranges.extend(start..end);
                }
                Some(MatchedRange::Chars(vec)) => {
                    ranges.extend(vec.iter());
                }
                _ => {}
            }
        }

        ranges.sort();
        ranges.dedup();
        MatchedItem::builder(item)
            .rank(rank)
            .matched_range(MatchedRange::Chars(ranges))
            .build()
    }
}

impl MatchEngine for AndEngine {
    fn match_item(&self, item: Arc<Item>) -> Option<MatchedItem> {
        // mock
        let mut results = vec![];
        for engine in &self.engines {
            let result = engine.match_item(Arc::clone(&item))?;
            results.push(result);
        }

        if results.is_empty() {
            None
        } else {
            Some(self.merge_matched_items(results))
        }
    }

    fn display(&self) -> String {
        format!(
            "(And: {})",
            self.engines.iter().map(|e| e.display()).collect::<Vec<_>>().join(", ")
        )
    }
}

//------------------------------------------------------------------------------
struct EngineFactory {}
impl EngineFactory {
    pub fn build(query: &str, mode: MatcherMode) -> Box<MatchEngine> {
        match mode {
            MatcherMode::Regex => Box::new(RegexEngine::builder(query).build()),
            MatcherMode::Fuzzy | MatcherMode::Exact => {
                if query.contains(' ') {
                    Box::new(AndEngine::builder(query, mode).build())
                } else {
                    EngineFactory::build_single(query, mode)
                }
            }
        }
    }

    fn build_single(query: &str, mode: MatcherMode) -> Box<MatchEngine> {
        if query.starts_with('\'') {
            if mode == MatcherMode::Exact {
                Box::new(FuzzyEngine::builder(&query[1..]).build())
            } else {
                Box::new(ExactEngine::builder(&query[1..], Algorithm::Exact).build())
            }
        } else if query.starts_with('^') {
            Box::new(ExactEngine::builder(&query[1..], Algorithm::PrefixExact).build())
        } else if query.starts_with('!') {
            if query.ends_with('$') {
                Box::new(ExactEngine::builder(&query[1..(query.len() - 1)], Algorithm::InverseSuffixExact).build())
            } else {
                Box::new(ExactEngine::builder(&query[1..], Algorithm::InverseExact).build())
            }
        } else if query.ends_with('$') {
            Box::new(ExactEngine::builder(&query[..(query.len() - 1)], Algorithm::SuffixExact).build())
        } else if mode == MatcherMode::Exact {
            Box::new(ExactEngine::builder(query, Algorithm::Exact).build())
        } else {
            Box::new(FuzzyEngine::builder(query).build())
        }
    }
}

//------------------------------------------------------------------------------
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RankCriteria {
    Score,
    Index,
    Begin,
    End,
    NegScore,
    NegIndex,
    NegBegin,
    NegEnd,
}

pub fn parse_criteria(text: &str) -> Option<RankCriteria> {
    match text.to_lowercase().as_ref() {
        "score" => Some(RankCriteria::Score),
        "index" => Some(RankCriteria::Index),
        "begin" => Some(RankCriteria::Begin),
        "end" => Some(RankCriteria::End),
        "-score" => Some(RankCriteria::NegScore),
        "-index" => Some(RankCriteria::NegIndex),
        "-begin" => Some(RankCriteria::NegBegin),
        "-end" => Some(RankCriteria::NegEnd),
        _ => None,
    }
}

#[cfg(test)]
mod test {
    use super::{EngineFactory, MatcherMode};

    #[test]
    fn test_engine_factory() {
        let x1 = EngineFactory::build("'abc | def ^gh ij | kl mn", MatcherMode::Fuzzy);
        assert_eq!(
            x1.display(),
            "(And: (Or: (Exact: abc), (Fuzzy: def)), (PrefixExact: gh), (Or: (Fuzzy: ij), (Fuzzy: kl)), (Fuzzy: mn))"
        );

        let x3 = EngineFactory::build("'abc | def ^gh ij | kl mn", MatcherMode::Regex);
        assert_eq!(x3.display(), "(Regex: 'abc | def ^gh ij | kl mn)");

        let x = EngineFactory::build("abc ", MatcherMode::Fuzzy);
        assert_eq!(x.display(), "(And: (Fuzzy: abc))");

        let x = EngineFactory::build("abc def", MatcherMode::Fuzzy);
        assert_eq!(x.display(), "(And: (Fuzzy: abc), (Fuzzy: def))");

        let x = EngineFactory::build("abc | def", MatcherMode::Fuzzy);
        assert_eq!(x.display(), "(And: (Or: (Fuzzy: abc), (Fuzzy: def)))");
    }
}

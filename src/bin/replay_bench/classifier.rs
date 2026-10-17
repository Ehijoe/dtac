//! The router scored as a bot classifier against the log's ground-truth labels,
//! independently of what the cache did with the scores.

use crate::metrics::ratio;
use dtac::heuristic_router::Evidence;
use serde::Serialize;

/// Threshold above which a request counts as flagged for precision/recall/FPR.
pub const THRESHOLD: f32 = 0.5;

/// Number of equal-width `bot_prob` bins in the per-class histograms.
pub const HISTOGRAM_BINS: usize = 20;

/// A request's ground truth, as precise as the log records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// A human on their first visit (fetches the favicon).
    HumanNew,
    /// A human whose browser already has the favicon cached.
    HumanReturning,
    /// A human in a log without visitor labels.
    Human,
    /// A mechanical scraper.
    BotNaive,
    /// A bot imitating browser sessions.
    BotStealthy,
    /// A bot in a log without `bot_kind`.
    Bot,
}

impl Label {
    /// The label for a record's `traffic_type`, `visitor` and `bot_kind` fields.
    pub fn of(traffic_type: &str, visitor: Option<&str>, bot_kind: Option<&str>) -> Self {
        if traffic_type == "bot" {
            match bot_kind {
                Some("naive") => Self::BotNaive,
                Some("stealthy") => Self::BotStealthy,
                _ => Self::Bot,
            }
        } else {
            match visitor {
                Some("new") => Self::HumanNew,
                Some("returning") => Self::HumanReturning,
                _ => Self::Human,
            }
        }
    }

    pub const fn is_bot(self) -> bool {
        matches!(self, Self::BotNaive | Self::BotStealthy | Self::Bot)
    }

    const fn name(self) -> &'static str {
        match self {
            Self::HumanNew => "human_new",
            Self::HumanReturning => "human_returning",
            Self::Human => "human",
            Self::BotNaive => "bot_naive",
            Self::BotStealthy => "bot_stealthy",
            Self::Bot => "bot",
        }
    }

    const ALL: [Self; 6] = [
        Self::HumanNew,
        Self::HumanReturning,
        Self::Human,
        Self::BotNaive,
        Self::BotStealthy,
        Self::Bot,
    ];
}

/// One scored measurement request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub bot_prob: f32,
    pub label: Label,
    /// Which heuristics fired.
    pub evidence: Evidence,
}

/// How often one heuristic fires on each class.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeuristicReport {
    pub name: &'static str,
    pub fired_human: u64,
    pub fired_bot: u64,
    /// `P(fired | human)`.
    pub rate_human: f64,
    /// `P(fired | bot)`.
    pub rate_bot: f64,
    /// Empirical log-likelihood ratio `ln(P(fired | bot) / P(fired | human))`, with 0.5
    /// added to each count (Haldane's correction) so it stays finite when a heuristic
    /// never fires on one class. This is the quantity the thesis says the weights are.
    pub llr: f64,
}

/// Reads one heuristic's flag out of an [`Evidence`].
type Fired = fn(&Evidence) -> bool;

/// The heuristics in [`Evidence`], by name.
const HEURISTICS: [(&str, Fired); 4] = [
    ("velocity", |e| e.velocity),
    ("datacenter_asn", |e| e.datacenter_asn),
    ("missing_assets", |e| e.missing_assets),
    ("missing_accept_language", |e| e.missing_accept_language),
];

/// Firing rates and empirical LLRs of every heuristic.
// Counts are far below 2^52, so f64 is exact here.
#[allow(clippy::cast_precision_loss)]
fn heuristic_reports(samples: &[Sample]) -> Vec<HeuristicReport> {
    let bots = samples.iter().filter(|s| s.label.is_bot()).count() as u64;
    let humans = samples.len() as u64 - bots;
    HEURISTICS
        .iter()
        .map(|&(name, fired)| {
            let count = |bot: bool| {
                samples
                    .iter()
                    .filter(|s| s.label.is_bot() == bot && fired(&s.evidence))
                    .count() as u64
            };
            let (fired_human, fired_bot) = (count(false), count(true));
            let smoothed = |k: u64, n: u64| (k as f64 + 0.5) / (n as f64 + 1.0);
            HeuristicReport {
                name,
                fired_human,
                fired_bot,
                rate_human: ratio(fired_human, humans),
                rate_bot: ratio(fired_bot, bots),
                llr: (smoothed(fired_bot, bots) / smoothed(fired_human, humans)).ln(),
            }
        })
        .collect()
}

/// Score distribution and flag rate for one [`Label`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LabelReport {
    pub label: &'static str,
    pub count: u64,
    pub mean_bot_prob: f64,
    /// Share of this label's requests scored above [`THRESHOLD`]: the FPR for human labels,
    /// the recall for bot labels.
    pub flagged_rate: f64,
}

/// Per-class `bot_prob` histograms over `[0, 1]` in [`HISTOGRAM_BINS`] equal bins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScoreHistogram {
    pub bins: usize,
    pub human: Vec<u64>,
    pub bot: Vec<u64>,
}

/// How well `bot_prob` separates bots from humans.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClassifierReport {
    pub samples: u64,
    /// Area under the ROC curve; `None` unless both classes are present.
    pub roc_auc: Option<f64>,
    pub threshold: f32,
    pub precision: f64,
    /// True-positive rate among bots.
    pub recall: f64,
    /// False-positive rate among humans.
    pub fpr: f64,
    pub mean_bot_prob_human: f64,
    pub mean_bot_prob_bot: f64,
    /// `mean_bot_prob_bot - mean_bot_prob_human`. The cache adds `1 - bot_prob` per hit,
    /// so this gap is what actually drives its behaviour.
    pub mean_gap: f64,
    /// Human FPR split by visitor type (`None` if the log has no such visitors). Returning
    /// visitors never fetch the favicon, so the Shy Guys heuristic flags them.
    pub fpr_new_visitors: Option<f64>,
    pub fpr_returning_visitors: Option<f64>,
    pub by_label: Vec<LabelReport>,
    pub histogram: ScoreHistogram,
    /// How often each heuristic fires on humans and bots.
    pub heuristics: Vec<HeuristicReport>,
}

/// Area under the ROC curve via the Mann-Whitney U statistic: sorts by score and
/// averages ranks across ties, so all-equal scores give 0.5. `None` unless both classes
/// are present.
// Counts and ranks are far below 2^52, so f64 is exact here.
#[allow(clippy::cast_precision_loss)]
pub fn roc_auc(samples: &[Sample]) -> Option<f64> {
    let mut scored: Vec<(f32, bool)> = samples
        .iter()
        .map(|s| (s.bot_prob, s.label.is_bot()))
        .collect();
    scored.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));

    let positives = scored.iter().filter(|(_, bot)| *bot).count();
    let negatives = scored.len() - positives;
    if positives == 0 || negatives == 0 {
        return None;
    }

    // Sum of the (1-based, tie-averaged) ranks of the positives.
    let mut rank_sum = 0.0;
    let mut i = 0;
    while i < scored.len() {
        let mut j = i;
        while j < scored.len() && scored[j].0.total_cmp(&scored[i].0).is_eq() {
            j += 1;
        }
        let average_rank = (i + 1 + j) as f64 / 2.0;
        let tied_positives = scored[i..j].iter().filter(|(_, bot)| *bot).count();
        rank_sum = average_rank.mul_add(tied_positives as f64, rank_sum);
        i = j;
    }
    let (p, n) = (positives as f64, negatives as f64);
    Some(p.mul_add(-(p + 1.0) / 2.0, rank_sum) / (p * n))
}

impl ClassifierReport {
    /// Scores `samples`, or `None` if there are none (e.g. a baseline run).
    // Counts are far below 2^52, so f64 is exact here.
    #[allow(clippy::cast_precision_loss)]
    pub fn new(samples: &[Sample]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let flagged = |s: &Sample| s.bot_prob > THRESHOLD;
        let count = |pred: &dyn Fn(&Sample) -> bool| samples.iter().filter(|s| pred(s)).count();
        let mean = |pred: &dyn Fn(&Sample) -> bool| {
            let (sum, n) = samples
                .iter()
                .filter(|s| pred(s))
                .fold((0.0, 0_u64), |(sum, n), s| {
                    (sum + f64::from(s.bot_prob), n + 1)
                });
            if n == 0 { 0.0 } else { sum / n as f64 }
        };

        let true_pos = count(&|s| s.label.is_bot() && flagged(s)) as u64;
        let false_pos = count(&|s| !s.label.is_bot() && flagged(s)) as u64;
        let bots = count(&|s| s.label.is_bot()) as u64;
        let humans = samples.len() as u64 - bots;

        let by_label: Vec<LabelReport> = Label::ALL
            .iter()
            .filter_map(|&label| {
                let n = count(&|s| s.label == label) as u64;
                (n > 0).then(|| LabelReport {
                    label: label.name(),
                    count: n,
                    mean_bot_prob: mean(&|s| s.label == label),
                    flagged_rate: ratio(count(&|s| s.label == label && flagged(s)) as u64, n),
                })
            })
            .collect();
        let flagged_rate_of = |label: Label| {
            by_label
                .iter()
                .find(|r| r.label == label.name())
                .map(|r| r.flagged_rate)
        };

        let mean_bot_prob_human = mean(&|s| !s.label.is_bot());
        let mean_bot_prob_bot = mean(&|s| s.label.is_bot());
        Some(Self {
            samples: samples.len() as u64,
            roc_auc: roc_auc(samples),
            threshold: THRESHOLD,
            precision: ratio(true_pos, true_pos + false_pos),
            recall: ratio(true_pos, bots),
            fpr: ratio(false_pos, humans),
            mean_bot_prob_human,
            mean_bot_prob_bot,
            mean_gap: mean_bot_prob_bot - mean_bot_prob_human,
            fpr_new_visitors: flagged_rate_of(Label::HumanNew),
            fpr_returning_visitors: flagged_rate_of(Label::HumanReturning),
            by_label,
            histogram: histogram(samples),
            heuristics: heuristic_reports(samples),
        })
    }

    pub fn print(&self) {
        let pct = |rate: f64| rate * 100.0;
        println!("ROUTER AS A CLASSIFIER (bot = positive, independent of the cache)");
        match self.roc_auc {
            Some(auc) => println!("ROC-AUC:                {auc:.4}"),
            None => println!("ROC-AUC:                n/a (only one class present)"),
        }
        println!(
            "At bot_prob > {}:      precision {:.2}%, recall {:.2}%, FPR {:.2}%",
            self.threshold,
            pct(self.precision),
            pct(self.recall),
            pct(self.fpr)
        );
        println!(
            "Mean bot_prob:          human {:.3}, bot {:.3} (gap {:.3})",
            self.mean_bot_prob_human, self.mean_bot_prob_bot, self.mean_gap
        );
        if let (Some(new), Some(returning)) = (self.fpr_new_visitors, self.fpr_returning_visitors) {
            println!(
                "Human FPR by visitor:   new {:.2}%, returning {:.2}%",
                pct(new),
                pct(returning)
            );
        }
        println!("Heuristic fires on:         human     bot  empirical LLR");
        for h in &self.heuristics {
            println!(
                "  {:<24}{:>6.2}%  {:>6.2}%  {:>+8.2}",
                h.name,
                pct(h.rate_human),
                pct(h.rate_bot),
                h.llr
            );
        }
    }
}

/// Bins each class's scores into [`HISTOGRAM_BINS`] equal-width bins over `[0, 1]`.
fn histogram(samples: &[Sample]) -> ScoreHistogram {
    let mut human = vec![0; HISTOGRAM_BINS];
    let mut bot = vec![0; HISTOGRAM_BINS];
    for s in samples {
        // `as` saturates, so out-of-range or NaN scores land in the first or last bin.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let bin = ((s.bot_prob * HISTOGRAM_BINS as f32) as usize).min(HISTOGRAM_BINS - 1);
        if s.label.is_bot() {
            bot[bin] += 1;
        } else {
            human[bin] += 1;
        }
    }
    ScoreHistogram {
        bins: HISTOGRAM_BINS,
        human,
        bot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(bot_prob: f32, label: Label) -> Sample {
        Sample {
            bot_prob,
            label,
            evidence: Evidence::default(),
        }
    }

    #[test]
    fn heuristic_llr_compares_firing_rates() {
        let header = Evidence {
            missing_accept_language: true,
            ..Evidence::default()
        };
        let with = |label, evidence| Sample {
            evidence,
            ..sample(0.5, label)
        };
        // 3 of 4 bots and 1 of 4 humans miss the header.
        let s = [
            with(Label::Bot, header),
            with(Label::Bot, header),
            with(Label::Bot, header),
            with(Label::Bot, Evidence::default()),
            with(Label::Human, header),
            with(Label::Human, Evidence::default()),
            with(Label::Human, Evidence::default()),
            with(Label::Human, Evidence::default()),
        ];
        let reports = heuristic_reports(&s);
        let h = reports
            .iter()
            .find(|h| h.name == "missing_accept_language")
            .unwrap();
        assert_eq!((h.fired_bot, h.fired_human), (3, 1));
        assert!((h.rate_bot - 0.75).abs() < 1e-12);
        assert!((h.llr - (3.5_f64 / 1.5).ln()).abs() < 1e-12);
        // A heuristic that never fires has an LLR of 0, not NaN.
        let asn = reports.iter().find(|h| h.name == "datacenter_asn").unwrap();
        assert!(asn.llr.abs() < 1e-12);
    }

    #[test]
    fn auc_of_perfectly_separated_scores_is_one() {
        let s = [
            sample(0.1, Label::Human),
            sample(0.2, Label::HumanNew),
            sample(0.8, Label::Bot),
            sample(0.9, Label::BotNaive),
        ];
        assert_eq!(roc_auc(&s), Some(1.0));
    }

    #[test]
    fn auc_of_reversed_scores_is_zero() {
        let s = [
            sample(0.9, Label::Human),
            sample(0.8, Label::Human),
            sample(0.2, Label::Bot),
            sample(0.1, Label::Bot),
        ];
        assert_eq!(roc_auc(&s), Some(0.0));
    }

    #[test]
    fn auc_of_tied_scores_is_one_half() {
        let s = [
            sample(0.5, Label::Human),
            sample(0.5, Label::Bot),
            sample(0.5, Label::Human),
            sample(0.5, Label::BotStealthy),
            sample(0.5, Label::Bot),
        ];
        assert_eq!(roc_auc(&s), Some(0.5));
    }

    #[test]
    fn auc_needs_both_classes() {
        assert_eq!(roc_auc(&[sample(0.3, Label::Human)]), None);
    }

    #[test]
    fn threshold_metrics_and_visitor_split() {
        let s = [
            sample(0.1, Label::HumanNew),
            sample(0.1, Label::HumanNew),
            sample(0.7, Label::HumanReturning), // false positive
            sample(0.2, Label::HumanReturning),
            sample(0.9, Label::BotNaive),    // true positive
            sample(0.3, Label::BotStealthy), // missed
        ];
        let r = ClassifierReport::new(&s).unwrap();
        assert!((r.precision - 0.5).abs() < 1e-12);
        assert!((r.recall - 0.5).abs() < 1e-12);
        assert!((r.fpr - 0.25).abs() < 1e-12);
        assert_eq!(r.fpr_new_visitors, Some(0.0));
        assert_eq!(r.fpr_returning_visitors, Some(0.5));
        assert!((r.mean_bot_prob_bot - 0.6).abs() < 1e-6);
        assert!((r.mean_gap - (0.6 - 0.275)).abs() < 1e-6);
        assert_eq!(r.histogram.human.iter().sum::<u64>(), 4);
        assert_eq!(r.histogram.bot[18], 1); // 0.9 -> bin 18 of 20
        let stealthy = r
            .by_label
            .iter()
            .find(|l| l.label == "bot_stealthy")
            .unwrap();
        assert!(stealthy.flagged_rate.abs() < 1e-12);
    }

    #[test]
    fn labels_come_from_record_fields() {
        assert_eq!(Label::of("bot", None, Some("stealthy")), Label::BotStealthy);
        assert_eq!(Label::of("bot", None, None), Label::Bot);
        assert_eq!(
            Label::of("human", Some("returning"), None),
            Label::HumanReturning
        );
        assert_eq!(Label::of("human", None, None), Label::Human);
    }
}

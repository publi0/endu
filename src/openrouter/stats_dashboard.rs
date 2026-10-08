//! Pure comparisons of measured requests. No inference from a vendor name or legacy counts.

use std::collections::{BTreeMap, BTreeSet};

use super::stats::{ModelRequests, RequestTotals, Totals};
use crate::providers::{ModelRef, Provider};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Group {
    #[default]
    Provider,
    Model,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    #[default]
    All,
    Live,
    Recorded,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Sort {
    #[default]
    Requests,
    Latency,
    Reliability,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonRow {
    pub id: String,
    pub label: String,
    pub provider: Option<Provider>,
    pub metrics: RequestTotals,
}

fn selected_mode(requests: &ModelRequests, mode: Mode) -> RequestTotals {
    match mode {
        Mode::All => requests.combined(),
        Mode::Live => requests.live.clone(),
        Mode::Recorded => requests.recorded.clone(),
    }
}

pub fn observed_providers(totals: &Totals) -> Vec<Provider> {
    totals
        .details
        .requests
        .keys()
        .chain(totals.models.keys())
        .chain(totals.model_latency.keys())
        .chain(totals.errors.values().flat_map(|models| models.keys()))
        .map(|id| ModelRef::parse(id).provider)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn combined_requests(totals: &Totals, mode: Mode, provider: Option<Provider>) -> RequestTotals {
    let mut combined = RequestTotals::default();
    for (id, requests) in &totals.details.requests {
        if provider.is_none_or(|provider| ModelRef::parse(id).provider == provider) {
            combined.merge(&selected_mode(requests, mode));
        }
    }
    combined
}

pub fn comparison_rows(
    totals: &Totals,
    group: Group,
    mode: Mode,
    provider: Option<Provider>,
    sort: Sort,
) -> Vec<ComparisonRow> {
    let mut rows = BTreeMap::<String, ComparisonRow>::new();
    for (id, requests) in &totals.details.requests {
        let model = ModelRef::parse(id);
        if provider.is_some_and(|provider| model.provider != provider) {
            continue;
        }
        let metrics = selected_mode(requests, mode);
        if metrics.attempts == 0 {
            continue;
        }
        let key = match group {
            Group::Provider => model.provider.id().to_owned(),
            Group::Model => model.key(),
        };
        let row = rows.entry(key.clone()).or_insert_with(|| ComparisonRow {
            id: key,
            label: match group {
                Group::Provider => model.provider.label().to_owned(),
                Group::Model => model_label(model),
            },
            provider: Some(model.provider),
            metrics: RequestTotals::default(),
        });
        row.metrics.merge(&metrics);
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|left, right| {
        let order = match sort {
            Sort::Requests => right.metrics.attempts.cmp(&left.metrics.attempts),
            Sort::Latency => match (
                left.metrics.latency.average_ms(),
                right.metrics.latency.average_ms(),
            ) {
                (Some(left), Some(right)) => left.cmp(&right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
            Sort::Reliability => {
                // Compare fractions without rounding rates or overflowing u64.
                let left_rate =
                    u128::from(left.metrics.successes) * u128::from(right.metrics.attempts);
                let right_rate =
                    u128::from(right.metrics.successes) * u128::from(left.metrics.attempts);
                right_rate.cmp(&left_rate)
            }
        };
        order
            .then_with(|| right.metrics.attempts.cmp(&left.metrics.attempts))
            .then_with(|| left.label.cmp(&right.label))
    });
    rows
}

fn model_label(model: ModelRef<'_>) -> String {
    let native = crate::providers::native_models()
        .iter()
        .find(|native| native.provider == model.provider && native.id == model.model);
    format!(
        "{} · {}",
        model.provider.label(),
        native.map_or(model.model, |native| native.name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requests(attempts: u64, samples: &[u64]) -> RequestTotals {
        let mut totals = RequestTotals {
            attempts,
            successes: samples.len() as u64,
            ..Default::default()
        };
        for sample in samples {
            totals.latency.record(*sample);
        }
        totals
    }

    fn fixture() -> Totals {
        let mut totals = Totals::default();
        totals.details.requests.insert(
            "microsoft/mai-transcribe-2".into(),
            ModelRequests {
                recorded: requests(4, &[100, 200]),
                ..Default::default()
            },
        );
        totals.details.requests.insert(
            "microsoft::MAI-Transcribe-2".into(),
            ModelRequests {
                recorded: requests(3, &[300, 600, 900]),
                ..Default::default()
            },
        );
        totals.details.requests.insert(
            "google::gemini-3.5-transcribe-live".into(),
            ModelRequests {
                live: requests(2, &[80, 120]),
                recorded: requests(1, &[900]),
            },
        );
        totals.details.requests.insert(
            "grok::grok-voice-transcribe-2.0".into(),
            ModelRequests {
                recorded: requests(5, &[]),
                ..Default::default()
            },
        );
        totals
    }

    #[test]
    fn provider_grouping_preserves_native_identity_and_weights_latency() {
        let totals = fixture();
        let rows = comparison_rows(&totals, Group::Provider, Mode::All, None, Sort::Requests);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "openrouter")
                .unwrap()
                .metrics
                .attempts,
            4
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "microsoft")
                .unwrap()
                .metrics
                .attempts,
            3
        );
        let google = rows.iter().find(|row| row.id == "google").unwrap();
        assert_eq!(google.metrics.latency.average_ms(), Some(1_100 / 3));
        assert_eq!(google.metrics.successes, 3);
        assert_eq!(google.metrics.latency.count, 3);
    }

    #[test]
    fn mode_filters_use_actual_capture_mode_even_for_realtime_model_ids() {
        let totals = fixture();
        let live = comparison_rows(&totals, Group::Model, Mode::Live, None, Sort::Requests);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].provider, Some(Provider::Google));
        assert_eq!(live[0].metrics.attempts, 2);
        let recorded = combined_requests(&totals, Mode::Recorded, Some(Provider::Google));
        assert_eq!(recorded.attempts, 1);
        assert_eq!(recorded.latency.average_ms(), Some(900));
        assert!(combined_requests(&totals, Mode::Live, Some(Provider::Microsoft)).attempts == 0);
    }

    #[test]
    fn sorting_keeps_unknown_latency_last_and_uses_unrounded_reliability() {
        let totals = fixture();
        let rows = comparison_rows(&totals, Group::Provider, Mode::All, None, Sort::Latency);
        assert_eq!(rows.first().unwrap().provider, Some(Provider::OpenRouter));
        assert_eq!(rows.last().unwrap().provider, Some(Provider::Grok));
        let rows = comparison_rows(&totals, Group::Provider, Mode::All, None, Sort::Reliability);
        assert_eq!(rows.last().unwrap().provider, Some(Provider::Grok));
        assert_eq!(rows[rows.len() - 2].provider, Some(Provider::OpenRouter));

        // Both display as 99%, but the smaller Google sample has the higher rate.
        // Rounded-rate sorting followed by volume would incorrectly prefer OpenRouter.
        let mut close_rates = Totals::default();
        for (model, attempts, successes) in [
            ("google::gemini-3.5-transcribe", 100, 99),
            ("openai/gpt-transcribe", 201, 198),
        ] {
            close_rates.details.requests.insert(
                model.into(),
                ModelRequests {
                    recorded: RequestTotals {
                        attempts,
                        successes,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        }
        let rows = comparison_rows(
            &close_rates,
            Group::Provider,
            Mode::All,
            None,
            Sort::Reliability,
        );
        assert_eq!(rows[0].provider, Some(Provider::Google));
    }

    #[test]
    fn legacy_totals_never_invent_request_counts_or_modes() {
        let mut totals = Totals::default();
        totals.models.insert("google/legacy".into(), 99);
        assert_eq!(observed_providers(&totals), [Provider::OpenRouter]);
        assert!(comparison_rows(&totals, Group::Model, Mode::All, None, Sort::Requests).is_empty());
        assert_eq!(combined_requests(&totals, Mode::All, None).attempts, 0);
    }
}

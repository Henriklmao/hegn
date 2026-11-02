//! Fuzzy project filtering for the picker.
//!
//! Matching is case-insensitive subsequence matching that rewards consecutive
//! characters, word boundaries and matches near the start.

use super::model::Project;

/// Fuzzy subsequence match. Higher is better; `None` means no match.
///
/// Matching is case-insensitive and rewards consecutive characters, word
/// boundaries and matches near the start.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    if needle.len() > hay.len() {
        return None;
    }

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut previous_match: Option<usize> = None;
    for &want in &needle {
        let mut found = None;
        while hi < hay.len() {
            if hay[hi] == want {
                found = Some(hi);
                break;
            }
            hi += 1;
        }
        let idx = found?;
        score += 10;
        if idx == 0 {
            score += 30;
        }
        if idx > 0 && matches!(hay[idx - 1], '/' | '-' | '_' | '.' | ' ') {
            score += 20;
        }
        if previous_match == Some(idx.saturating_sub(1)) {
            score += 30;
        }
        score -= (idx.saturating_sub(previous_match.map(|p| p + 1).unwrap_or(0))) as i32;
        previous_match = Some(idx);
        hi = idx + 1;
    }
    Some(score)
}

/// Filter indices of `projects` by `query`, preserving recency for empty
/// queries and ranking by fuzzy score otherwise.
pub fn filter_projects(projects: &[Project], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        let mut indices: Vec<usize> = (0..projects.len()).collect();
        indices.sort_by(|&a, &b| {
            projects[b]
                .last_opened
                .cmp(&projects[a].last_opened)
                .then_with(|| {
                    projects[a]
                        .name
                        .to_lowercase()
                        .cmp(&projects[b].name.to_lowercase())
                })
        });
        return indices;
    }

    let mut scored: Vec<(usize, i32)> = projects
        .iter()
        .enumerate()
        .filter_map(|(i, project)| {
            let name = fuzzy_score(query, &project.name);
            let path = fuzzy_score(query, &project.path.to_string_lossy());
            match (name, path) {
                (Some(a), Some(b)) => Some((i, a.max(b) + 25)),
                (Some(a), None) => Some((i, a + 25)),
                (None, Some(b)) => Some((i, b)),
                (None, None) => None,
            }
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{GitSummary, Project};
    use chrono::Utc;
    use std::path::PathBuf;

    #[test]
    fn fuzzy_matches_subsequences_and_ranks_boundaries() {
        assert!(fuzzy_score("hgn", "hegn-tui").is_some());
        assert!(fuzzy_score("tui", "hegn-tui").is_some());
        assert!(fuzzy_score("xyz", "hegn-tui").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        let boundary = fuzzy_score("tui", "hegn-tui").unwrap();
        let scattered = fuzzy_score("tui", "t-u-i").unwrap();
        assert!(boundary > scattered);
    }

    #[test]
    fn filters_and_orders_by_recency() {
        let now = Utc::now();
        let projects = vec![
            Project {
                name: "alpha".into(),
                path: PathBuf::from("/p/alpha"),
                git: GitSummary::default(),
                last_opened: None,
            },
            Project {
                name: "hegn-tui".into(),
                path: PathBuf::from("/p/hegn-tui"),
                git: GitSummary::default(),
                last_opened: Some(now),
            },
        ];
        let filtered = filter_projects(&projects, "");
        assert_eq!(filtered[0], 1);
        let filtered = filter_projects(&projects, "hgn");
        assert_eq!(filtered[0], 1);
        assert!(filter_projects(&projects, "zzz").is_empty());
    }
}

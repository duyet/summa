/**
 * Cost distribution: when per-model costs are missing (Codex/ccusage models
 * often only report a parent totalCost), distribute proportionally by
 * outputTokens (or inputTokens as fallback). The last breakdown absorbs
 * rounding so Σ(row costs) == parentCost exactly.
 */

/// Input breakdowns that will have their `cost` field adjusted.
pub struct BreakdownForCost {
    pub output_tokens: u64,
    pub input_tokens: u64,
    pub cost: f64,
}

/// Distribute `parent_cost` across `breakdowns` proportionally.
///
/// - No-op when per-model costs already sum > 0 (caller already has them).
/// - No-op when `parent_cost <= 0`.
/// - Weights by `outputTokens`; falls back to `inputTokens` when all outputs are 0.
/// - When weight is 0 (no tokens), all cost goes to the first entry.
/// - The last entry absorbs the remainder to guarantee `Σ == parent_cost`.
pub fn distribute_cost(breakdowns: &mut [BreakdownForCost], parent_cost: f64) {
    let total_cost: f64 = breakdowns.iter().map(|b| b.cost).sum();
    if total_cost > 0.0 || parent_cost <= 0.0 {
        return;
    }

    let total_output: u64 = breakdowns.iter().map(|b| b.output_tokens).sum();
    let total_input: u64 = breakdowns.iter().map(|b| b.input_tokens).sum();
    // One basis for the whole distribution. Picking per-row would compare
    // different units mid-loop: a row with no output would fall back to its
    // input tokens while the total stays on output tokens, taking a share
    // larger than the parent cost and pushing the last row negative.
    let use_output = total_output > 0;
    let weight = if use_output {
        total_output as f64
    } else {
        total_input as f64
    };

    if weight == 0.0 {
        if !breakdowns.is_empty() {
            breakdowns[0].cost = parent_cost;
        }
        return;
    }

    let mut remaining = parent_cost;
    let len = breakdowns.len();
    for (i, bd) in breakdowns.iter_mut().enumerate() {
        if i == len - 1 {
            // Last entry absorbs remainder to avoid rounding drift.
            bd.cost = round8(remaining);
        } else {
            let w = if use_output {
                bd.output_tokens
            } else {
                bd.input_tokens
            } as f64;
            let share = parent_cost * (w / weight);
            bd.cost = round8(share);
            remaining -= bd.cost;
        }
    }
}

/// Round to 8 decimal places (matches TS `Math.round(x * 1e8) / 1e8`).
fn round8(v: f64) -> f64 {
    (v * 1e8).round() / 1e8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bds(costs: &[f64], outputs: &[u64], inputs: &[u64]) -> Vec<BreakdownForCost> {
        (0..costs.len())
            .map(|i| BreakdownForCost { cost: costs[i], output_tokens: outputs[i], input_tokens: inputs[i] })
            .collect()
    }

    #[test]
    fn no_op_when_per_model_costs_present() {
        let mut bds_arr = bds(&[1.0, 2.0], &[10, 20], &[1, 2]);
        distribute_cost(&mut bds_arr, 99.0);
        assert_eq!(bds_arr.iter().map(|b| b.cost).collect::<Vec<_>>(), vec![1.0, 2.0]);
    }

    #[test]
    fn no_op_when_parent_cost_zero() {
        let mut bds_arr = bds(&[0.0, 0.0], &[10, 20], &[1, 2]);
        distribute_cost(&mut bds_arr, 0.0);
        assert_eq!(bds_arr.iter().map(|b| b.cost).collect::<Vec<_>>(), vec![0.0, 0.0]);
    }

    #[test]
    fn weights_by_output_last_absorbs_rounding() {
        let mut bds_arr = bds(&[0.0, 0.0, 0.0], &[1, 1, 1], &[0, 0, 0]);
        distribute_cost(&mut bds_arr, 1.0);
        let sum: f64 = bds_arr.iter().map(|b| b.cost).sum();
        assert!((sum - 1.0).abs() < 1e-8);
    }

    #[test]
    fn falls_back_to_input_when_all_outputs_zero() {
        let mut bds_arr = bds(&[0.0, 0.0], &[0, 0], &[3, 1]);
        distribute_cost(&mut bds_arr, 4.0);
        assert!((bds_arr[0].cost - 3.0).abs() < 1e-8);
        assert!((bds_arr[1].cost - 1.0).abs() < 1e-8);
    }

    #[test]
    fn zero_tokens_all_cost_to_first() {
        let mut bds_arr = bds(&[0.0, 0.0], &[0, 0], &[0, 0]);
        distribute_cost(&mut bds_arr, 5.0);
        assert_eq!(bds_arr[0].cost, 5.0);
        assert_eq!(bds_arr[1].cost, 0.0);
    }

    #[test]
    fn mixed_zero_output_row_does_not_inflate_its_share_or_go_negative() {
        // A model that consumed input but produced no output (cache warming, a
        // turn cut off before the reply) has output_tokens == 0. The total
        // weight is the sum of *output* tokens, so counting that row's input
        // tokens as its weight compares different units: it takes a share
        // larger than the whole parent cost, and the last row — which absorbs
        // the remainder — goes negative. Negative cost rows then flow into
        // daily rollups and cancel real spend.
        let mut bds_arr = bds(&[0.0, 0.0], &[0, 100], &[1000, 1000]);
        distribute_cost(&mut bds_arr, 1.0);
        for (i, b) in bds_arr.iter().enumerate() {
            assert!(b.cost >= 0.0, "row {i} has negative cost {}", b.cost);
        }
        let sum: f64 = bds_arr.iter().map(|b| b.cost).sum();
        assert!((sum - 1.0).abs() < 1e-8, "sum was {sum}");
    }

    #[test]
    fn single_weight_basis_is_used_for_every_row() {
        // With a shared output basis, a zero-output row earns a zero share
        // rather than silently switching units mid-distribution.
        let mut bds_arr = bds(&[0.0, 0.0], &[0, 100], &[1000, 1000]);
        distribute_cost(&mut bds_arr, 1.0);
        assert!((bds_arr[0].cost - 0.0).abs() < 1e-8, "got {}", bds_arr[0].cost);
        assert!((bds_arr[1].cost - 1.0).abs() < 1e-8, "got {}", bds_arr[1].cost);
    }

    #[test]
    fn empty_slice_no_op() {
        let mut empty: Vec<BreakdownForCost> = vec![];
        distribute_cost(&mut empty, 5.0);
        assert!(empty.is_empty());
    }
}

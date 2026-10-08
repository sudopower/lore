/// Seat and span limits for each pricing plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Plan { Free, Pro, Team }

pub struct Limits { pub seats: u32, pub spans_per_day: u64 }

/// Returns the seat limit and daily span limit of a plan.
pub fn limits(plan: Plan) -> Limits {
    match plan {
        Plan::Free => Limits { seats: 1, spans_per_day: 10_000 },
        Plan::Pro => Limits { seats: 5, spans_per_day: 1_000_000 },
        Plan::Team => Limits { seats: 25, spans_per_day: 10_000_000 },
    }
}

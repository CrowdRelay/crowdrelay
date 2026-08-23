//! What a show actually costs to play.
//!
//! `live_opportunities` already refuses a gig whose margin is negative. It just
//! has no idea what the cost is: `estimated_cost_minor` arrives from outside and
//! nobody computes it, so every "can we take this gig" answer has only ever been
//! as good as a number somebody typed. This module computes it from facts.
//!
//! The case that motivates every line here: home is Wrocław, the offer is 500 km
//! away, the band travels in two cars because five people and a backline do not
//! fit in one, and the question is whether the fee still leaves money. Two cars
//! is roughly double the fuel and double the tolls, and a model that assumes one
//! vehicle answers that question wrong in the direction that costs the band.
//!
//! Two deliberate refusals:
//!
//! - **Missing inputs are not guessed.** No distance, no fuel price, no vehicle
//!   capacity means [`CostEvidence::Insufficient`] naming the field. Filling a
//!   missing distance with a band average is how an agent talks a band into a
//!   loss-making drive.
//! - **Merch and bar revenue are not modelled.** They are real and they are
//!   unpredictable. An agent that books a losing show because it assumed merch
//!   would cover the difference is worse than one that says no.
//!
//! Integer arithmetic throughout, money in minor units, intermediate products in
//! `i128` so a long drive in a cheap currency cannot overflow into a bargain.

use serde::{Deserialize, Serialize};

/// One vehicle the band actually owns or hires.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct VehicleProfile {
    /// Seats including the driver.
    pub seats: u8,
    /// Usable cargo volume in litres, after seats are occupied.
    pub cargo_litres: u32,
    /// Consumption in centilitres per 100 km — `850` is 8.5 l/100 km. Centilitres
    /// so a realistic figure survives integer arithmetic without a float.
    pub fuel_centilitres_per_100km: u32,
}

impl Default for VehicleProfile {
    fn default() -> Self {
        Self {
            seats: 5,
            cargo_litres: 900,
            fuel_centilitres_per_100km: 800,
        }
    }
}

/// Everything about the band and its rates that does not change per gig.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TourEconomicsPolicy {
    /// All-in road cost per 100 km of **round trip**, the way a band actually
    /// quotes it: fuel, tolls and wear in one number somebody can state from
    /// experience. When set, this replaces the fuel-and-tolls arithmetic below,
    /// which needs three figures nobody keeps in their head.
    pub transport_minor_per_100km_round_trip: i64,
    /// How many vehicles that rate was quoted for. A band that says "200 zł per
    /// 100 km" usually means their normal convoy, so a trip needing more
    /// vehicles scales the rate rather than pretending the extra car is free.
    pub transport_rate_covers_vehicles: u8,
    pub vehicle: VehicleProfile,
    /// Hard ceiling on vehicles. Beyond this the trip is not a logistics
    /// question any more and a human should be looking at it.
    pub max_vehicles: u8,
    /// People travelling: band plus anyone who has to be there.
    pub crew_size: u8,
    /// Backline and merch volume in litres.
    pub backline_litres: u32,
    pub fuel_price_minor_per_litre: i64,
    /// Tolls and road charges per kilometre, per vehicle.
    pub toll_minor_per_km: i64,
    pub accommodation_minor_per_room_night: i64,
    pub crew_per_room: u8,
    pub per_diem_minor_per_person_day: i64,
    /// Costs paid whether or not the show sells: rehearsal, loading, wear.
    pub fixed_overhead_minor: i64,
    /// One-way distance at or beyond which the band stays overnight. A policy
    /// the operator sets, not an inference the domain makes.
    pub overnight_threshold_km: u32,
    /// What the band must clear above cost for a show to be worth playing.
    pub minimum_margin_minor: i64,
}

impl Default for TourEconomicsPolicy {
    fn default() -> Self {
        Self {
            transport_minor_per_100km_round_trip: 0,
            transport_rate_covers_vehicles: 1,
            vehicle: VehicleProfile::default(),
            max_vehicles: 3,
            crew_size: 5,
            backline_litres: 1_200,
            // Zero means "not configured yet" and is reported as insufficient
            // evidence rather than as free fuel.
            fuel_price_minor_per_litre: 0,
            toll_minor_per_km: 0,
            accommodation_minor_per_room_night: 0,
            crew_per_room: 2,
            per_diem_minor_per_person_day: 0,
            fixed_overhead_minor: 0,
            overnight_threshold_km: 350,
            minimum_margin_minor: 0,
        }
    }
}

/// What this particular offer involves.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShowLogistics {
    /// One-way road distance from home base. `None` when nobody has supplied it.
    pub distance_km: Option<u32>,
    /// Nights away, when the promoter or operator has stated it. `None` falls
    /// back to the overnight threshold, which is policy rather than a guess.
    pub nights_away: Option<u8>,
    /// Fee offered, before any application fee.
    pub offered_fee_minor: i64,
    /// What it costs to apply or submit, if anything.
    pub application_fee_minor: i64,
}

/// The input the model could not do without.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingInput {
    Distance,
    /// Neither an all-in road rate nor a fuel price is configured.
    TransportRate,
    VehicleCapacity,
    CrewSize,
    /// The trip needs beds and nobody has said what a bed costs.
    AccommodationRate,
}

impl MissingInput {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Distance => "distance_km",
            Self::TransportRate => "transport_minor_per_100km_round_trip",
            Self::VehicleCapacity => "vehicle_capacity",
            Self::CrewSize => "crew_size",
            Self::AccommodationRate => "accommodation_minor_per_room_night",
        }
    }

    /// What an operator has to do about it. The point of naming the field is
    /// that somebody can fix it in a minute.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::Distance => "supply the one-way road distance from home base",
            Self::TransportRate => {
                "set the all-in road cost per 100 km round trip, or a fuel price and consumption"
            }
            Self::VehicleCapacity => "set vehicle seats, cargo litres and consumption",
            Self::CrewSize => "set how many people travel",
            Self::AccommodationRate => {
                "set the room rate, or raise the overnight threshold if the band never pays for beds"
            }
        }
    }
}

/// An itemised cost, or an honest refusal to produce one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "evidence")]
pub enum CostEvidence {
    /// Every input was present. The itemisation is carried so an operator can
    /// see *why* a gig was refused instead of only that it was.
    Complete(ShowCost),
    Insufficient {
        missing: MissingInput,
    },
}

impl CostEvidence {
    #[must_use]
    pub const fn cost(self) -> Option<ShowCost> {
        match self {
            Self::Complete(cost) => Some(cost),
            Self::Insufficient { .. } => None,
        }
    }

    /// True when the offer clears cost plus the band's minimum margin.
    ///
    /// Unknown cost is never profitable. A gig whose economics cannot be
    /// computed is prepared for a human, never auto-anything.
    #[must_use]
    pub const fn clears_floor(self) -> bool {
        match self {
            Self::Complete(cost) => cost.net_margin_minor >= 0,
            Self::Insufficient { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportBasis {
    /// One all-in rate the operator stated.
    FlatRate,
    /// Fuel from consumption and price, plus tolls per kilometre.
    FuelAndTolls,
}

/// Every line of what the trip costs, and what is left of the fee.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ShowCost {
    /// Which road-cost model produced `transport_minor`.
    pub transport_basis: TransportBasis,
    /// The whole road cost, whichever way it was reached. `fuel_minor` and
    /// `tolls_minor` are the itemisation and are zero under a flat rate.
    pub transport_minor: i64,
    /// Derived from crew and backline against one vehicle's capacity — never
    /// assumed to be one.
    pub vehicles: u8,
    pub round_trip_km: u32,
    pub nights_away: u8,
    pub rooms: u8,
    pub fuel_minor: i64,
    pub tolls_minor: i64,
    pub accommodation_minor: i64,
    pub per_diem_minor: i64,
    pub overhead_minor: i64,
    pub total_cost_minor: i64,
    /// Fee minus every cost above, minus the application fee. Negative means
    /// the band pays to play.
    pub net_margin_minor: i64,
    /// The fee at which this show becomes worth playing: cost plus the minimum
    /// margin plus the application fee. What Phase 8 negotiates up from.
    pub walk_away_fee_minor: i64,
}

/// Integer ceiling division, saturating.
const fn div_ceil_u32(value: u32, divisor: u32) -> u32 {
    if divisor == 0 {
        return 0;
    }
    value.div_ceil(divisor)
}

/// Multiplies in `i128` and narrows once. Every money product here is
/// distance × rate × vehicles, which overflows `i64` far more easily than it
/// looks once a currency has small units.
fn money(product: i128) -> i64 {
    if product > i128::from(i64::MAX) {
        i64::MAX
    } else if product < i128::from(i64::MIN) {
        i64::MIN
    } else {
        product as i64
    }
}

/// How many vehicles this crew and this backline actually need.
///
/// The seats answer and the cargo answer are both computed and the larger wins:
/// four people with a full backline still need two vehicles, and six people with
/// no gear also need two.
#[must_use]
pub const fn required_vehicles(policy: &TourEconomicsPolicy) -> Option<u8> {
    if policy.crew_size == 0 {
        return None;
    }
    if policy.vehicle.seats == 0 || policy.vehicle.cargo_litres == 0 {
        return None;
    }
    let for_people = div_ceil_u32(policy.crew_size as u32, policy.vehicle.seats as u32);
    let for_gear = div_ceil_u32(policy.backline_litres, policy.vehicle.cargo_litres);
    let needed = if for_people > for_gear {
        for_people
    } else {
        for_gear
    };
    let needed = if needed == 0 { 1 } else { needed };
    let capped = if needed > policy.max_vehicles as u32 {
        policy.max_vehicles as u32
    } else {
        needed
    };
    if capped == 0 {
        None
    } else {
        Some(capped as u8)
    }
}

/// Computes what one show costs and what the offer leaves.
#[must_use]
pub fn estimate_show_cost(logistics: &ShowLogistics, policy: &TourEconomicsPolicy) -> CostEvidence {
    let Some(distance_km) = logistics.distance_km else {
        return CostEvidence::Insufficient {
            missing: MissingInput::Distance,
        };
    };
    if policy.crew_size == 0 {
        return CostEvidence::Insufficient {
            missing: MissingInput::CrewSize,
        };
    }
    let Some(vehicles) = required_vehicles(policy) else {
        return CostEvidence::Insufficient {
            missing: MissingInput::VehicleCapacity,
        };
    };
    let flat_rate = policy.transport_minor_per_100km_round_trip > 0;
    let itemised =
        policy.vehicle.fuel_centilitres_per_100km > 0 && policy.fuel_price_minor_per_litre > 0;
    if !flat_rate && !itemised {
        // Free road is not a thing. An unset rate is an unanswered question,
        // and answering it with zero makes every distant gig look profitable.
        return CostEvidence::Insufficient {
            missing: MissingInput::TransportRate,
        };
    }

    let round_trip_km = distance_km.saturating_mul(2);
    let vehicles_i128 = i128::from(vehicles);

    let (transport_basis, transport_minor, fuel_minor, tolls_minor) = if flat_rate {
        // The rate is quoted for a normal convoy, so a trip that needs more
        // vehicles scales it. Fewer never discounts it: the quote is what the
        // band knows their road costs to be.
        let covers = i128::from(policy.transport_rate_covers_vehicles.max(1));
        let scale = if vehicles_i128 > covers {
            vehicles_i128
        } else {
            covers
        };
        (
            TransportBasis::FlatRate,
            money(
                i128::from(round_trip_km)
                    * i128::from(policy.transport_minor_per_100km_round_trip)
                    * scale
                    / (100 * covers),
            ),
            0,
            0,
        )
    } else {
        // centilitres per 100 km × km ÷ 100 gives centilitres; ÷ 100 again gives
        // litres. Kept as one division so the rounding happens once.
        let fuel_minor = money(
            i128::from(round_trip_km)
                * i128::from(policy.vehicle.fuel_centilitres_per_100km)
                * i128::from(policy.fuel_price_minor_per_litre)
                * vehicles_i128
                / 10_000,
        );
        let tolls_minor =
            money(i128::from(round_trip_km) * i128::from(policy.toll_minor_per_km) * vehicles_i128);
        (
            TransportBasis::FuelAndTolls,
            money(i128::from(fuel_minor) + i128::from(tolls_minor)),
            fuel_minor,
            tolls_minor,
        )
    };

    let nights_away = logistics.nights_away.unwrap_or({
        if distance_km >= policy.overnight_threshold_km {
            1
        } else {
            0
        }
    });
    if nights_away > 0 && policy.accommodation_minor_per_room_night <= 0 {
        // Somebody has to sleep somewhere. A band that genuinely never pays for
        // beds says so by raising the overnight threshold, not by leaving the
        // rate at zero and hoping.
        return CostEvidence::Insufficient {
            missing: MissingInput::AccommodationRate,
        };
    }
    let rooms = if policy.crew_per_room == 0 {
        policy.crew_size
    } else {
        div_ceil_u32(policy.crew_size as u32, policy.crew_per_room as u32)
            .try_into()
            .unwrap_or(u8::MAX)
    };
    let accommodation_minor = money(
        i128::from(rooms)
            * i128::from(nights_away)
            * i128::from(policy.accommodation_minor_per_room_night),
    );
    // Nights plus the show day itself: a same-day return still feeds people.
    let days = i128::from(nights_away) + 1;
    let per_diem_minor = money(
        i128::from(policy.crew_size) * days * i128::from(policy.per_diem_minor_per_person_day),
    );

    let total_cost_minor = money(
        i128::from(transport_minor)
            + i128::from(accommodation_minor)
            + i128::from(per_diem_minor)
            + i128::from(policy.fixed_overhead_minor),
    );
    let net_margin_minor = money(
        i128::from(logistics.offered_fee_minor)
            - i128::from(total_cost_minor)
            - i128::from(logistics.application_fee_minor),
    );
    let walk_away_fee_minor = money(
        i128::from(total_cost_minor)
            + i128::from(policy.minimum_margin_minor)
            + i128::from(logistics.application_fee_minor),
    );

    CostEvidence::Complete(ShowCost {
        transport_basis,
        transport_minor,
        vehicles,
        round_trip_km,
        nights_away,
        rooms,
        fuel_minor,
        tolls_minor,
        accommodation_minor,
        per_diem_minor,
        overhead_minor: policy.fixed_overhead_minor,
        total_cost_minor,
        net_margin_minor,
        walk_away_fee_minor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configured band: five people, a real backline, Polish road costs in
    /// grosze. Numbers are plausible, not authoritative — the operator's own
    /// config replaces them.
    fn band() -> TourEconomicsPolicy {
        TourEconomicsPolicy {
            // Flat rate off in this fixture so the itemised arithmetic stays
            // under test; the flat path has its own tests below.
            transport_minor_per_100km_round_trip: 0,
            transport_rate_covers_vehicles: 2,
            vehicle: VehicleProfile {
                seats: 5,
                cargo_litres: 900,
                fuel_centilitres_per_100km: 800,
            },
            max_vehicles: 3,
            crew_size: 5,
            backline_litres: 1_200,
            fuel_price_minor_per_litre: 650,
            toll_minor_per_km: 12,
            accommodation_minor_per_room_night: 18_000,
            crew_per_room: 2,
            per_diem_minor_per_person_day: 6_000,
            fixed_overhead_minor: 20_000,
            overnight_threshold_km: 350,
            minimum_margin_minor: 50_000,
        }
    }

    fn complete(evidence: CostEvidence) -> ShowCost {
        match evidence {
            CostEvidence::Complete(cost) => cost,
            CostEvidence::Insufficient { missing } => {
                panic!("expected a cost, missing {}", missing.as_str())
            }
        }
    }

    #[test]
    fn five_people_and_a_backline_need_two_cars() {
        // Seats alone say one car. The backline is what forces the second, and
        // a model that only counted people would halve the fuel.
        let policy = band();
        assert_eq!(required_vehicles(&policy), Some(2));

        let seats_only = TourEconomicsPolicy {
            backline_litres: 0,
            ..policy
        };
        assert_eq!(required_vehicles(&seats_only), Some(1));

        let people_only = TourEconomicsPolicy {
            crew_size: 6,
            backline_litres: 0,
            ..policy
        };
        assert_eq!(required_vehicles(&people_only), Some(2));
    }

    #[test]
    fn the_five_hundred_kilometre_offer_is_costed_line_by_line() {
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(500),
                nights_away: None,
                offered_fee_minor: 400_000,
                application_fee_minor: 0,
            },
            &band(),
        ));

        assert_eq!(cost.vehicles, 2);
        assert_eq!(cost.round_trip_km, 1_000);
        // 500 km one way is past the 350 km threshold, so one night.
        assert_eq!(cost.nights_away, 1);
        assert_eq!(cost.rooms, 3);
        // 1000 km × 8 l/100km × 6.50 zł × 2 cars = 1040 zł.
        assert_eq!(cost.fuel_minor, 104_000);
        assert_eq!(cost.tolls_minor, 24_000);
        assert_eq!(cost.accommodation_minor, 54_000);
        // 5 people × 2 days × 60 zł.
        assert_eq!(cost.per_diem_minor, 60_000);
        assert_eq!(cost.overhead_minor, 20_000);
        assert_eq!(cost.total_cost_minor, 262_000);
        assert_eq!(cost.net_margin_minor, 138_000);
        // Cost plus the 500 zł the band wants to clear.
        assert_eq!(cost.walk_away_fee_minor, 312_000);
    }

    #[test]
    fn a_second_car_roughly_doubles_the_road_costs() {
        // The whole reason the model exists: this is the difference between a
        // gig that pays and one that does not.
        let logistics = ShowLogistics {
            distance_km: Some(500),
            nights_away: None,
            offered_fee_minor: 300_000,
            application_fee_minor: 0,
        };
        let two_cars = complete(estimate_show_cost(&logistics, &band()));
        let one_car = complete(estimate_show_cost(
            &logistics,
            &TourEconomicsPolicy {
                backline_litres: 0,
                crew_size: 4,
                ..band()
            },
        ));

        assert_eq!(one_car.vehicles, 1);
        assert_eq!(two_cars.fuel_minor, one_car.fuel_minor * 2);
        assert_eq!(two_cars.tolls_minor, one_car.tolls_minor * 2);
        assert!(two_cars.total_cost_minor > one_car.total_cost_minor);
    }

    #[test]
    fn a_distant_gig_at_a_local_fee_is_a_loss_and_says_so() {
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(500),
                nights_away: None,
                offered_fee_minor: 150_000,
                application_fee_minor: 0,
            },
            &band(),
        ));
        assert!(cost.net_margin_minor < 0);
        assert!(cost.walk_away_fee_minor > 150_000);
    }

    #[test]
    fn a_nearby_gig_needs_no_hotel() {
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(80),
                nights_away: None,
                offered_fee_minor: 200_000,
                application_fee_minor: 0,
            },
            &band(),
        ));
        assert_eq!(cost.nights_away, 0);
        assert_eq!(cost.accommodation_minor, 0);
        // One day of per diem, because people still eat on a day trip.
        assert_eq!(cost.per_diem_minor, 30_000);
    }

    #[test]
    fn a_stated_overnight_count_beats_the_threshold() {
        // A promoter saying "you are on at 1am, stay two nights" is a fact; the
        // threshold is only the fallback.
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(80),
                nights_away: Some(2),
                offered_fee_minor: 200_000,
                application_fee_minor: 0,
            },
            &band(),
        ));
        assert_eq!(cost.nights_away, 2);
        assert_eq!(cost.accommodation_minor, 108_000);
    }

    #[test]
    fn the_application_fee_comes_out_of_the_margin_and_raises_the_floor() {
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(500),
                nights_away: None,
                offered_fee_minor: 400_000,
                application_fee_minor: 30_000,
            },
            &band(),
        ));
        assert_eq!(cost.net_margin_minor, 108_000);
        assert_eq!(cost.walk_away_fee_minor, 342_000);
    }

    #[test]
    fn an_unknown_distance_is_never_filled_in() {
        // Filling this with a band average is how an agent talks a band into a
        // loss-making drive.
        let evidence = estimate_show_cost(
            &ShowLogistics {
                distance_km: None,
                ..ShowLogistics::default()
            },
            &band(),
        );
        assert_eq!(
            evidence,
            CostEvidence::Insufficient {
                missing: MissingInput::Distance
            }
        );
        assert!(!evidence.clears_floor());
        assert_eq!(evidence.cost(), None);
    }

    #[test]
    fn an_unset_road_rate_is_not_a_free_road() {
        // Zero would make every distant gig look profitable.
        let evidence = estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(500),
                ..ShowLogistics::default()
            },
            &TourEconomicsPolicy {
                fuel_price_minor_per_litre: 0,
                ..band()
            },
        );
        assert_eq!(
            evidence,
            CostEvidence::Insufficient {
                missing: MissingInput::TransportRate
            }
        );
    }

    #[test]
    fn an_unconfigured_vehicle_or_crew_refuses_rather_than_assumes_one_car() {
        let no_capacity = estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(100),
                ..ShowLogistics::default()
            },
            &TourEconomicsPolicy {
                vehicle: VehicleProfile {
                    seats: 0,
                    cargo_litres: 0,
                    fuel_centilitres_per_100km: 800,
                },
                ..band()
            },
        );
        assert_eq!(
            no_capacity,
            CostEvidence::Insufficient {
                missing: MissingInput::VehicleCapacity
            }
        );

        let no_crew = estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(100),
                ..ShowLogistics::default()
            },
            &TourEconomicsPolicy {
                crew_size: 0,
                ..band()
            },
        );
        assert_eq!(
            no_crew,
            CostEvidence::Insufficient {
                missing: MissingInput::CrewSize
            }
        );
    }

    #[test]
    fn unknown_cost_is_never_treated_as_profitable() {
        for evidence in [
            CostEvidence::Insufficient {
                missing: MissingInput::Distance,
            },
            CostEvidence::Insufficient {
                missing: MissingInput::TransportRate,
            },
        ] {
            assert!(!evidence.clears_floor());
        }
        let profitable = estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(50),
                nights_away: None,
                offered_fee_minor: 500_000,
                application_fee_minor: 0,
            },
            &band(),
        );
        assert!(profitable.clears_floor());
    }

    #[test]
    fn vehicles_are_capped_so_a_convoy_reaches_a_human() {
        let policy = TourEconomicsPolicy {
            crew_size: 40,
            max_vehicles: 3,
            ..band()
        };
        assert_eq!(required_vehicles(&policy), Some(3));
    }

    #[test]
    fn every_missing_input_names_itself_and_its_remedy() {
        for missing in [
            MissingInput::Distance,
            MissingInput::TransportRate,
            MissingInput::VehicleCapacity,
            MissingInput::CrewSize,
        ] {
            assert!(!missing.as_str().is_empty());
            assert!(!missing.remedy().is_empty());
        }
    }

    #[test]
    fn extreme_inputs_saturate_instead_of_wrapping_into_a_bargain() {
        // An overflow here would turn an impossible trip into a cheap one.
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(u32::MAX),
                nights_away: Some(u8::MAX),
                offered_fee_minor: 0,
                application_fee_minor: 0,
            },
            &TourEconomicsPolicy {
                fuel_price_minor_per_litre: i64::MAX,
                toll_minor_per_km: i64::MAX,
                accommodation_minor_per_room_night: i64::MAX,
                per_diem_minor_per_person_day: i64::MAX,
                fixed_overhead_minor: i64::MAX,
                ..band()
            },
        ));
        assert_eq!(cost.total_cost_minor, i64::MAX);
        assert!(cost.net_margin_minor < 0);
    }

    /// The operator's own declared figures: an all-in 200 zł per 100 km of
    /// round trip quoted for the usual two cars, six people, 300 zł of overhead
    /// and a 500 zł minimum the band wants to clear.
    fn declared() -> TourEconomicsPolicy {
        TourEconomicsPolicy {
            transport_minor_per_100km_round_trip: 20_000,
            transport_rate_covers_vehicles: 2,
            vehicle: VehicleProfile {
                seats: 5,
                cargo_litres: 900,
                // The fallback if the flat rate is ever cleared: 8 l/100 km at
                // 8 zł a litre. Two cars at those figures is 128 zł per 100 km,
                // so the 200 zł flat rate carries about 72 zł of tolls and wear.
                fuel_centilitres_per_100km: 800,
            },
            max_vehicles: 3,
            crew_size: 6,
            backline_litres: 1_200,
            fuel_price_minor_per_litre: 800,
            toll_minor_per_km: 0,
            accommodation_minor_per_room_night: 18_000,
            crew_per_room: 2,
            per_diem_minor_per_person_day: 6_000,
            fixed_overhead_minor: 30_000,
            overnight_threshold_km: 350,
            minimum_margin_minor: 50_000,
        }
    }

    #[test]
    fn the_declared_flat_rate_costs_the_five_hundred_kilometre_trip() {
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(500),
                nights_away: None,
                offered_fee_minor: 400_000,
                application_fee_minor: 0,
            },
            &declared(),
        ));

        assert_eq!(cost.transport_basis, TransportBasis::FlatRate);
        // 1000 km round trip at 200 zł per 100 km, the rate already covering
        // the two cars this trip needs.
        assert_eq!(cost.transport_minor, 200_000);
        assert_eq!(cost.vehicles, 2);
        // The itemisation is empty under a flat rate; the whole road cost is
        // the one number the operator stated.
        assert_eq!(cost.fuel_minor, 0);
        assert_eq!(cost.tolls_minor, 0);
        assert_eq!(cost.rooms, 3);
        assert_eq!(cost.accommodation_minor, 54_000);
        assert_eq!(cost.per_diem_minor, 72_000);
        assert_eq!(cost.overhead_minor, 30_000);
        assert_eq!(cost.total_cost_minor, 356_000);
        assert_eq!(cost.net_margin_minor, 44_000);
        // Cost plus the 500 zł the band will not go below.
        assert_eq!(cost.walk_away_fee_minor, 406_000);
    }

    #[test]
    fn a_flat_rate_scales_when_the_trip_needs_more_cars_than_it_covers() {
        // The quote is what the band knows two cars to cost. A third car is
        // not free just because the rate did not mention it.
        let policy = TourEconomicsPolicy {
            crew_size: 12,
            backline_litres: 2_500,
            ..declared()
        };
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(100),
                nights_away: Some(0),
                offered_fee_minor: 0,
                application_fee_minor: 0,
            },
            &policy,
        ));
        assert_eq!(cost.vehicles, 3);
        // 200 km round trip is 400 zł for two cars, half as much again for three.
        assert_eq!(cost.transport_minor, 60_000);
    }

    #[test]
    fn a_flat_rate_is_never_discounted_below_what_it_covers() {
        // A solo acoustic run does not make the band's stated road cost cheaper
        // than the band says it is.
        let policy = TourEconomicsPolicy {
            crew_size: 1,
            backline_litres: 0,
            ..declared()
        };
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(100),
                nights_away: Some(0),
                offered_fee_minor: 0,
                application_fee_minor: 0,
            },
            &policy,
        ));
        assert_eq!(cost.vehicles, 1);
        assert_eq!(cost.transport_minor, 40_000);
    }

    #[test]
    fn the_flat_rate_replaces_the_fuel_arithmetic_rather_than_adding_to_it() {
        let logistics = ShowLogistics {
            distance_km: Some(300),
            nights_away: Some(0),
            offered_fee_minor: 0,
            application_fee_minor: 0,
        };
        let flat = complete(estimate_show_cost(&logistics, &declared()));
        let itemised = complete(estimate_show_cost(
            &logistics,
            &TourEconomicsPolicy {
                transport_minor_per_100km_round_trip: 0,
                ..declared()
            },
        ));

        assert_eq!(flat.transport_basis, TransportBasis::FlatRate);
        assert_eq!(itemised.transport_basis, TransportBasis::FuelAndTolls);
        // 600 km round trip: 1200 zł flat, against 600 × 8 l/100 km × 8 zł × 2
        // cars = 768 zł of fuel with no tolls configured.
        assert_eq!(flat.transport_minor, 120_000);
        assert_eq!(itemised.transport_minor, 76_800);
        assert_eq!(itemised.fuel_minor, 76_800);
        assert_eq!(
            itemised.total_cost_minor,
            itemised.transport_minor + itemised.per_diem_minor + itemised.overhead_minor
        );
    }

    #[test]
    fn a_trip_that_needs_beds_refuses_when_nobody_priced_a_bed() {
        // Zero rooms cost would make an overnight look like a day trip. A band
        // that never pays for beds raises the overnight threshold instead.
        let evidence = estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(500),
                nights_away: None,
                offered_fee_minor: 400_000,
                application_fee_minor: 0,
            },
            &TourEconomicsPolicy {
                accommodation_minor_per_room_night: 0,
                ..declared()
            },
        );
        assert_eq!(
            evidence,
            CostEvidence::Insufficient {
                missing: MissingInput::AccommodationRate
            }
        );

        // The same band that never pays for beds, saying so properly.
        let never_stays = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(500),
                nights_away: None,
                offered_fee_minor: 400_000,
                application_fee_minor: 0,
            },
            &TourEconomicsPolicy {
                accommodation_minor_per_room_night: 0,
                overnight_threshold_km: 20_000,
                ..declared()
            },
        ));
        assert_eq!(never_stays.nights_away, 0);
        assert_eq!(never_stays.accommodation_minor, 0);
    }

    #[test]
    fn a_day_trip_needs_no_bed_rate_at_all() {
        let cost = complete(estimate_show_cost(
            &ShowLogistics {
                distance_km: Some(120),
                nights_away: None,
                offered_fee_minor: 200_000,
                application_fee_minor: 0,
            },
            &TourEconomicsPolicy {
                accommodation_minor_per_room_night: 0,
                ..declared()
            },
        ));
        assert_eq!(cost.nights_away, 0);
        assert_eq!(cost.accommodation_minor, 0);
    }
}

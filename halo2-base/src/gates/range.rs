use crate::{
    gates::{
        flex_gate::{FlexGateConfig, GateStrategy, MAX_PHASE},
        GateInstructions,
    },
    utils::{decompose_fe_to_u64_limbs, value_to_option, ScalarField},
    AssignedValue,
    QuantumCell::{self, Constant, Existing, Witness},
};
use crate::{
    halo2_proofs::{
        circuit::{Layouter, Value},
        plonk::{
            Advice, Column, ConstraintSystem, Error, SecondPhase, Selector, TableColumn, ThirdPhase,
        },
        poly::Rotation,
    },
    utils::PrimeField,
};
use std::cmp::Ordering;

use super::{Context, RangeInstructions};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RangeStrategy {
    Vertical, // vanilla implementation with vertical basic gate(s)
    // CustomVerticalShort, // vertical basic gate(s) and vertical custom range gates of length 2,3
    PlonkPlus,
    // CustomHorizontal, // vertical basic gate and dedicated horizontal custom gate
}

#[derive(Clone, Debug)]
pub struct RangeConfig<F: ScalarField> {
    // `lookup_advice` are special advice columns only used for lookups
    //
    // If `strategy` is `Vertical` or `CustomVertical`:
    // * If `gate` has only 1 advice column, enable lookups for that column, in which case `lookup_advice` is empty
    // * Otherwise, add some user-specified number of `lookup_advice` columns
    //   * In this case, we don't even need a selector so `q_lookup` is empty
    // If `strategy` is `CustomHorizontal`:
    // * TODO
    pub lookup_advice: [Vec<Column<Advice>>; MAX_PHASE],
    pub q_lookup: Vec<Option<Selector>>,
    pub lookup: TableColumn,
    pub lookup_bits: usize,
    pub limb_bases: Vec<QuantumCell<'static, 'static, F>>,
    // selector for custom range gate
    // `q_range[k][i]` stores the selector for a custom range gate of length `k`
    // pub q_range: HashMap<usize, Vec<Selector>>,
    pub gate: FlexGateConfig<F>,
    strategy: RangeStrategy,
    pub context_id: usize,
}

impl<F: ScalarField> RangeConfig<F> {
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        range_strategy: RangeStrategy,
        num_advice: &[usize],
        num_lookup_advice: &[usize],
        num_fixed: usize,
        lookup_bits: usize,
        context_id: usize,
        // params.k()
        circuit_degree: usize,
    ) -> Self {
        assert!(lookup_bits <= 28);
        let lookup = meta.lookup_table_column();

        let gate = FlexGateConfig::configure(
            meta,
            match range_strategy {
                RangeStrategy::Vertical => GateStrategy::Vertical,
                RangeStrategy::PlonkPlus => GateStrategy::PlonkPlus,
            },
            num_advice,
            num_fixed,
            context_id,
            circuit_degree,
        );

        // For now, we apply the same range lookup table to each phase
        let mut q_lookup = Vec::new();
        let mut lookup_advice = [(); MAX_PHASE].map(|_| Vec::new());
        for (phase, &num_columns) in num_lookup_advice.iter().enumerate() {
            // if num_columns is set to 0, then we assume you do not want to perform any lookups in that phase
            if num_advice[phase] == 1 && num_columns != 0 {
                q_lookup.push(Some(meta.complex_selector()));
            } else {
                q_lookup.push(None);
                for _ in 0..num_columns {
                    let a = match phase {
                        0 => meta.advice_column(),
                        1 => meta.advice_column_in(SecondPhase),
                        2 => meta.advice_column_in(ThirdPhase),
                        _ => panic!("Currently RangeConfig only supports {MAX_PHASE} phases"),
                    };
                    meta.enable_equality(a);
                    lookup_advice[phase].push(a);
                }
            }
        }

        let limb_base = F::from(1u64 << lookup_bits);
        let mut running_base = limb_base;
        let num_bases = F::NUM_BITS as usize / lookup_bits;
        let mut limb_bases = Vec::with_capacity(num_bases + 1);
        limb_bases.extend([Constant(F::one()), Constant(running_base)]);
        for _ in 2..=num_bases {
            running_base *= &limb_base;
            limb_bases.push(Constant(running_base));
        }

        let config = Self {
            lookup_advice,
            q_lookup,
            lookup,
            lookup_bits,
            limb_bases,
            gate,
            strategy: range_strategy,
            context_id,
        };
        config.create_lookup(meta);

        config
    }

    fn create_lookup(&self, meta: &mut ConstraintSystem<F>) {
        for (phase, q_l) in self.q_lookup.iter().enumerate() {
            if let Some(q) = q_l {
                meta.lookup("lookup", |meta| {
                    let q = meta.query_selector(*q);
                    // there should only be 1 advice column in phase `phase`
                    let a =
                        meta.query_advice(self.gate.basic_gates[phase][0].value, Rotation::cur());
                    vec![(q * a, self.lookup)]
                });
            }
        }
        for la in self.lookup_advice.iter().flat_map(|advices| advices.iter()) {
            meta.lookup("lookup wo selector", |meta| {
                let a = meta.query_advice(*la, Rotation::cur());
                vec![(a, self.lookup)]
            });
        }
    }

    pub fn load_lookup_table(&self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
        layouter.assign_table(
            || format!("{} bit lookup", self.lookup_bits),
            |mut table| {
                for idx in 0..(1u32 << self.lookup_bits) {
                    table.assign_cell(
                        || "lookup table",
                        self.lookup,
                        idx as usize,
                        || Value::known(F::from(idx as u64)),
                    )?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    /// Call this at the end of a phase to assign cells to special columns for lookup arguments
    ///
    /// returns total number of lookup cells assigned
    pub fn finalize(&self, ctx: &mut Context<'_, F>) -> usize {
        ctx.copy_and_lookup_cells(self.lookup_advice[ctx.current_phase].clone())
    }

    /// assuming this is called when ctx.region is not in shape mode
    /// `offset` is the offset of the cell in `ctx.region`
    /// `offset` is only used if there is a single advice column
    fn enable_lookup<'a>(&self, ctx: &mut Context<'a, F>, acell: AssignedValue<'a, F>) {
        let phase = ctx.current_phase();
        if let Some(q) = &self.q_lookup[phase] {
            q.enable(&mut ctx.region, acell.row()).expect("enable selector should not fail");
        } else {
            ctx.cells_to_lookup.push(acell);
        }
    }

    // returns the limbs
    fn range_check_simple<'a>(
        &self,
        ctx: &mut Context<'a, F>,
        a: &AssignedValue<'a, F>,
        range_bits: usize,
        limbs_assigned: &mut Vec<AssignedValue<'a, F>>,
    ) {
        let k = (range_bits + self.lookup_bits - 1) / self.lookup_bits;
        // println!("range check {} bits {} len", range_bits, k);
        let rem_bits = range_bits % self.lookup_bits;

        assert!(self.limb_bases.len() >= k);
        if k == 1 {
            limbs_assigned.clear();
            limbs_assigned.push(a.clone())
        } else {
            let acc = match value_to_option(a.value()) {
                Some(a) => {
                    let limbs = decompose_fe_to_u64_limbs(a, k, self.lookup_bits)
                        .into_iter()
                        .map(|x| Witness(Value::known(F::from(x))));
                    self.gate.inner_product_left(
                        ctx,
                        limbs,
                        self.limb_bases[..k].iter().cloned(),
                        limbs_assigned,
                    )
                }
                _ => self.gate.inner_product_left(
                    ctx,
                    vec![Witness(Value::unknown()); k],
                    self.limb_bases[..k].iter().cloned(),
                    limbs_assigned,
                ),
            };
            // the inner product above must equal `a`
            ctx.region.constrain_equal(a.cell(), acc.cell());
        };
        assert_eq!(limbs_assigned.len(), k);

        // range check all the limbs
        for limb in limbs_assigned.iter() {
            self.enable_lookup(ctx, limb.clone());
        }

        // additional constraints for the last limb if rem_bits != 0
        match rem_bits.cmp(&1) {
            // we want to check x := limbs[k-1] is boolean
            // we constrain x*(x-1) = 0 + x * x - x == 0
            // | 0 | x | x | x |
            Ordering::Equal => {
                self.gate.assert_bit(ctx, &limbs_assigned[k - 1]);
            }
            Ordering::Greater => {
                let mult_val = self.gate.get_field_element(1u64 << (self.lookup_bits - rem_bits));
                let check = self.gate.assign_region_last(
                    ctx,
                    vec![
                        Constant(F::zero()),
                        Existing(&limbs_assigned[k - 1]),
                        Constant(mult_val),
                        Witness(limbs_assigned[k - 1].value().map(|limb| mult_val * limb)),
                    ],
                    vec![(0, None)],
                );
                self.enable_lookup(ctx, check);
            }
            _ => {}
        }
    }

    /// breaks up `a` into smaller pieces to lookup and stores them in `limbs_assigned`
    ///
    /// this is an internal function to avoid memory re-allocation of `limbs_assigned`
    pub fn range_check_limbs<'a>(
        &self,
        ctx: &mut Context<'a, F>,
        a: &AssignedValue<'a, F>,
        range_bits: usize,
        limbs_assigned: &mut Vec<AssignedValue<'a, F>>,
    ) {
        assert_ne!(range_bits, 0);
        #[cfg(feature = "display")]
        {
            let key = format!(
                "range check length {}",
                (range_bits + self.lookup_bits - 1) / self.lookup_bits
            );
            let count = ctx.op_count.entry(key).or_insert(0);
            *count += 1;
        }
        match self.strategy {
            RangeStrategy::Vertical | RangeStrategy::PlonkPlus => {
                self.range_check_simple(ctx, a, range_bits, limbs_assigned)
            }
        }
    }

    /// assume `a` has been range checked already to `limb_bits` bits
    pub fn get_last_bit<'a>(
        &self,
        ctx: &mut Context<'a, F>,
        a: &AssignedValue<'a, F>,
        limb_bits: usize,
    ) -> AssignedValue<'a, F> {
        let a_v = a.value();
        let bit_v = a_v.map(|a| {
            let a = a.get_lower_32();
            if a ^ 1 == 0 {
                F::zero()
            } else {
                F::one()
            }
        });
        let two = self.gate.get_field_element(2u64);
        let h_v = a.value().zip(bit_v).map(|(a, b)| (*a - b) * two.invert().unwrap());
        let assignments = self.gate.assign_region_smart(
            ctx,
            vec![Witness(bit_v), Witness(h_v), Constant(two), Existing(a)],
            vec![0],
            vec![],
            vec![],
        );

        self.range_check(ctx, &assignments[1], limb_bits - 1);
        assignments.into_iter().next().unwrap()
    }
}

impl<F: ScalarField> RangeInstructions<F> for RangeConfig<F> {
    type Gate = FlexGateConfig<F>;

    fn gate(&self) -> &Self::Gate {
        &self.gate
    }
    fn strategy(&self) -> RangeStrategy {
        self.strategy
    }

    fn lookup_bits(&self) -> usize {
        self.lookup_bits
    }

    fn range_check<'a>(
        &self,
        ctx: &mut Context<'a, F>,
        a: &AssignedValue<'a, F>,
        range_bits: usize,
    ) {
        let tmp = ctx.preallocated_vec_to_assign();
        self.range_check_limbs(ctx, a, range_bits, &mut tmp.as_ref().borrow_mut());
    }

    /// Warning: This may fail silently if a or b have more than num_bits
    fn check_less_than<'a>(
        &self,
        ctx: &mut Context<'a, F>,
        a: QuantumCell<'_, 'a, F>,
        b: QuantumCell<'_, 'a, F>,
        num_bits: usize,
    ) {
        let pow_of_two = self.gate.pow_of_two[num_bits];
        let check_cell = match self.strategy {
            RangeStrategy::Vertical => {
                let shift_a_val = a.value().map(|av| pow_of_two + av);
                // | a + 2^(num_bits) - b | b | 1 | a + 2^(num_bits) | - 2^(num_bits) | 1 | a |
                let cells = vec![
                    Witness(shift_a_val - b.value()),
                    b,
                    Constant(F::one()),
                    Witness(shift_a_val),
                    Constant(-pow_of_two),
                    Constant(F::one()),
                    a,
                ];
                let assigned_cells =
                    self.gate.assign_region(ctx, cells, vec![(0, None), (3, None)]);
                assigned_cells.into_iter().next().unwrap()
            }
            RangeStrategy::PlonkPlus => {
                // | a | 1 | b | a + 2^{num_bits} - b |
                // selectors:
                // | 1 | 0 | 0 |
                // | 0 | 2^{num_bits} | -1 |
                let out_val = Value::known(pow_of_two) + a.value() - b.value();
                let assigned_cells = self.gate.assign_region(
                    ctx,
                    vec![a, Constant(F::one()), b, Witness(out_val)],
                    vec![(0, Some([F::zero(), pow_of_two, -F::one()]))],
                );
                assigned_cells.into_iter().nth(3).unwrap()
            }
        };

        self.range_check(ctx, &check_cell, num_bits);
    }

    /// Warning: This may fail silently if a or b have more than num_bits
    fn is_less_than<'a>(
        &self,
        ctx: &mut Context<'a, F>,
        a: QuantumCell<'_, 'a, F>,
        b: QuantumCell<'_, 'a, F>,
        num_bits: usize,
    ) -> AssignedValue<'a, F> {
        // TODO: optimize this for PlonkPlus strategy
        let k = (num_bits + self.lookup_bits - 1) / self.lookup_bits;
        let padded_bits = k * self.lookup_bits;
        let pow_padded = self.gate.pow_of_two[padded_bits];

        let shift_a_val = a.value().map(|av| pow_padded + av);
        let shifted_val = shift_a_val - b.value();
        let shifted_cell = match self.strategy {
            RangeStrategy::Vertical => {
                let assignments = self.gate.assign_region_smart(
                    ctx,
                    vec![
                        Witness(shifted_val),
                        b,
                        Constant(F::one()),
                        Witness(shift_a_val),
                        Constant(-pow_padded),
                        Constant(F::one()),
                        a,
                    ],
                    vec![0, 3],
                    vec![],
                    vec![],
                );
                assignments.into_iter().next().unwrap()
            }
            RangeStrategy::PlonkPlus => self.gate.assign_region_last(
                ctx,
                vec![a, Constant(pow_padded), b, Witness(shifted_val)],
                vec![(0, Some([F::zero(), F::one(), -F::one()]))],
            ),
        };

        // check whether a - b + 2^padded_bits < 2^padded_bits ?
        // since assuming a, b < 2^padded_bits we are guaranteed a - b + 2^padded_bits < 2^{padded_bits + 1}
        let limbs = ctx.preallocated_vec_to_assign();
        self.range_check_limbs(
            ctx,
            &shifted_cell,
            padded_bits + self.lookup_bits,
            &mut limbs.borrow_mut(),
        );
        let res = self.gate().is_zero(ctx, limbs.borrow().get(k).unwrap());
        res
    }
}

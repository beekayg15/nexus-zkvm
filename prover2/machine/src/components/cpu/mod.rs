use num_traits::One;
use stwo_prover::{
    constraint_framework::{EvalAtRow, RelationEntry},
    core::{
        backend::simd::{
            column::BaseColumn,
            m31::{PackedBaseField, LOG_N_LANES},
            SimdBackend,
        },
        fields::{m31::BaseField, qm31::SecureField},
        poly::{circle::CircleEvaluation, BitReversedOrder},
        ColumnVec,
    },
};

use nexus_vm::riscv::BuiltinOpcode;
use nexus_vm_prover_trace::{
    builder::{FinalizedTrace, TraceBuilder},
    component::{ComponentTrace, FinalizedColumn},
    eval::TraceEval,
    original_base_column, preprocessed_base_column, preprocessed_trace_eval,
    program::ProgramStep,
    trace_eval,
};

use super::utils::u32_to_16bit_parts_le;
use crate::{
    framework::BuiltInComponent,
    lookups::{
        AllLookupElements, ComponentLookupElements, CpuToInstLookupElements, LogupTraceBuilder,
        ProgramExecutionLookupElements,
    },
    side_note::SideNote,
};

mod columns;
use columns::{Column, PreprocessedColumn};

pub struct Cpu;

impl Cpu {
    fn generate_trace_row(
        trace: &mut TraceBuilder<Column>,
        row_idx: usize,
        vm_step: ProgramStep,
        _side_note: &mut SideNote,
    ) {
        let step = &vm_step.step;
        let pc = step.pc;

        let pc_bytes = pc.to_le_bytes();
        let pc_aux = pc_bytes[0] / 4;

        trace.fill_columns_bytes(row_idx, &pc_bytes, Column::Pc);
        trace.fill_columns(row_idx, pc_aux, Column::PcAux);

        let (flag_column, opcode) = match step.instruction.opcode.builtin() {
            Some(BuiltinOpcode::ADD) => (Column::IsAdd, BuiltinOpcode::ADD),
            Some(BuiltinOpcode::ADDI) => (Column::IsAddI, BuiltinOpcode::ADDI),
            _ => {
                panic!("Unsupported opcode: {:?}", step.instruction.opcode);
            }
        };
        trace.fill_columns(row_idx, true, flag_column);
        trace.fill_columns(row_idx, opcode.raw(), Column::Opcode);

        let a_val = vm_step
            .get_result()
            .expect("instructions with no output are unsupported");
        let b_val = vm_step.get_value_b();
        let (c_val, _) = vm_step.get_value_c();

        trace.fill_columns(row_idx, a_val, Column::AVal);
        trace.fill_columns(row_idx, b_val, Column::BVal);
        trace.fill_columns(row_idx, c_val, Column::CVal);
    }

    /// Combines two 8-bit limbs into a single 16-bit column.
    fn get_16bit_column(log_size: u32, low: FinalizedColumn, high: FinalizedColumn) -> BaseColumn {
        let mut data = Vec::with_capacity(1 << (log_size - LOG_N_LANES));
        for vec_row in 0..1 << (log_size - LOG_N_LANES) {
            data.push(
                low.at(vec_row) + high.at(vec_row) * PackedBaseField::from(BaseField::from(1 << 8)),
            );
        }
        BaseColumn::from_simd(data)
    }
}

impl BuiltInComponent for Cpu {
    type PreprocessedColumn = PreprocessedColumn;

    type MainColumn = Column;

    type LookupElements = (CpuToInstLookupElements, ProgramExecutionLookupElements);

    fn generate_preprocessed_trace(&self, log_size: u32) -> FinalizedTrace {
        let (clk_low, clk_high): (Vec<BaseField>, Vec<BaseField>) = (1..=(1 << log_size))
            .map(|clk| {
                let [clk_low, clk_high] = u32_to_16bit_parts_le(clk);
                (
                    BaseField::from(clk_low as u32),
                    BaseField::from(clk_high as u32),
                )
            })
            .unzip();
        let clk_low = BaseColumn::from_iter(clk_low);
        let clk_high = BaseColumn::from_iter(clk_high);
        FinalizedTrace {
            cols: vec![clk_low, clk_high],
            log_size,
        }
    }

    fn generate_main_trace(&self, side_note: &mut SideNote) -> FinalizedTrace {
        let num_steps = side_note.num_program_steps();
        let log_size = num_steps.next_power_of_two().ilog2().max(LOG_N_LANES);

        let mut trace = TraceBuilder::new(log_size);
        for (row_idx, program_step) in side_note.iter_program_steps().enumerate() {
            Self::generate_trace_row(&mut trace, row_idx, program_step, side_note);
        }

        for row_idx in num_steps..1 << log_size {
            trace.fill_columns(row_idx, true, Column::IsPad);
        }
        trace.finalize()
    }

    fn generate_interaction_trace(
        &self,
        component_trace: ComponentTrace,
        _side_note: &SideNote,
        lookup_elements: &AllLookupElements,
    ) -> (
        ColumnVec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>>,
        SecureField,
    ) {
        let (rel_cpu_to_inst, rel_cont_prog_exec) = Self::LookupElements::get(lookup_elements);
        let log_size = component_trace.log_size();
        let mut logup_trace_builder = LogupTraceBuilder::new(log_size);

        let [is_add] = original_base_column!(component_trace, Column::IsAdd);
        let [is_addi] = original_base_column!(component_trace, Column::IsAddI);
        let [is_pad] = original_base_column!(component_trace, Column::IsPad);

        let pc = original_base_column!(component_trace, Column::Pc);
        let a_val = original_base_column!(component_trace, Column::AVal);
        let b_val = original_base_column!(component_trace, Column::BVal);
        let c_val = original_base_column!(component_trace, Column::CVal);

        let [opcode] = original_base_column!(component_trace, Column::Opcode);

        let [clk_low, clk_high] =
            preprocessed_base_column!(component_trace, PreprocessedColumn::Clk);
        let pc_low = Self::get_16bit_column(log_size, pc[0], pc[1]);
        let pc_high = Self::get_16bit_column(log_size, pc[2], pc[3]);
        // consume(rel-cont-prog-exec, 1 − is-pad, (clk, pc))
        logup_trace_builder.add_to_relation_with(
            &rel_cont_prog_exec,
            [is_pad],
            |[is_pad]| (is_pad - PackedBaseField::one()).into(),
            &[clk_low, clk_high, (&pc_low).into(), (&pc_high).into()],
        );
        // TODO: replace multiplicity with virtual flags.
        //
        // provide(rel-cpu-to-inst, is-type-u + is-type-j + is-load + is-type-s + is-type-b + is-alu,
        //      (clk, opcode, pc, a-val, b-val, c-val))
        logup_trace_builder.add_to_relation_with(
            &rel_cpu_to_inst,
            [is_add, is_addi],
            |[is_add, is_addi]| (is_add + is_addi).into(),
            &[
                [
                    clk_low,
                    clk_high,
                    opcode,
                    (&pc_low).into(),
                    (&pc_high).into(),
                ]
                .as_slice(),
                &a_val,
                &b_val,
                &c_val,
            ]
            .concat(),
        );

        logup_trace_builder.finalize()
    }

    fn add_constraints<E: EvalAtRow>(
        &self,
        eval: &mut E,
        trace_eval: TraceEval<Self::PreprocessedColumn, Self::MainColumn, E>,
        lookup_elements: &Self::LookupElements,
    ) {
        let [is_add] = trace_eval!(trace_eval, Column::IsAdd);
        let [is_addi] = trace_eval!(trace_eval, Column::IsAddI);
        let [is_pad] = trace_eval!(trace_eval, Column::IsPad);

        let pc = trace_eval!(trace_eval, Column::Pc);
        let [pc_aux] = trace_eval!(trace_eval, Column::PcAux);

        eval.add_constraint(pc_aux * BaseField::from(4) - pc[0].clone());

        let [opcode] = trace_eval!(trace_eval, Column::Opcode);
        let a_val = trace_eval!(trace_eval, Column::AVal);
        let b_val = trace_eval!(trace_eval, Column::BVal);
        let c_val = trace_eval!(trace_eval, Column::CVal);

        // Logup Interactions
        let (rel_cpu_to_inst, rel_cont_prog_exec) = lookup_elements;

        // Lookup 16 bits
        let [clk_low, clk_high] = preprocessed_trace_eval!(trace_eval, PreprocessedColumn::Clk);

        let pc_low = pc[0].clone() + pc[1].clone() * BaseField::from(1 << 8);
        let pc_high = pc[2].clone() + pc[3].clone() * BaseField::from(1 << 8);

        // consume(rel-cont-prog-exec, 1 − is-pad, (clk, pc))
        eval.add_to_relation(RelationEntry::new(
            rel_cont_prog_exec,
            (is_pad.clone() - E::F::one()).into(),
            &[
                clk_low.clone(),
                clk_high.clone(),
                pc_low.clone(),
                pc_high.clone(),
            ],
        ));

        // TODO: replace multiplicity with virtual flags.
        //
        // provide(rel-cpu-to-inst, is-type-u + is-type-j + is-load + is-type-s + is-type-b + is-alu,
        //      (clk, opcode, pc, a-val, b-val, c-val))
        eval.add_to_relation(RelationEntry::new(
            rel_cpu_to_inst,
            (is_add + is_addi).into(),
            &[
                [
                    clk_low.clone(),
                    clk_high.clone(),
                    opcode.clone(),
                    pc_low.clone(),
                    pc_high.clone(),
                ]
                .as_slice(),
                &a_val,
                &b_val,
                &c_val,
            ]
            .concat(),
        ));

        eval.finalize_logup_in_pairs();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::framework::test_utils::assert_component;
    use nexus_vm::{
        riscv::{BasicBlock, BuiltinOpcode, Instruction, Opcode},
        trace::k_trace_direct,
    };

    #[test]
    fn assert_cpu_constraints() {
        let basic_block = vec![BasicBlock::new(vec![
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADDI), 1, 0, 1),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 2, 1, 0),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 3, 2, 1),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 4, 3, 2),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 5, 4, 3),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 6, 5, 4),
        ])];
        let (_view, program_trace) =
            k_trace_direct(&basic_block, 1).expect("error generating trace");

        assert_component(Cpu, &program_trace);
    }
}

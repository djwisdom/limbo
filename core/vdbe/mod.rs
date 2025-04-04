//! The virtual database engine (VDBE).
//!
//! The VDBE is a register-based virtual machine that execute bytecode
//! instructions that represent SQL statements. When an application prepares
//! an SQL statement, the statement is compiled into a sequence of bytecode
//! instructions that perform the needed operations, such as reading or
//! writing to a b-tree, sorting, or aggregating data.
//!
//! The instruction set of the VDBE is similar to SQLite's instruction set,
//! but with the exception that bytecodes that perform I/O operations are
//! return execution back to the caller instead of blocking. This is because
//! Limbo is designed for applications that need high concurrency such as
//! serverless runtimes. In addition, asynchronous I/O makes storage
//! disaggregation easier.
//!
//! You can find a full list of SQLite opcodes at:
//!
//! https://www.sqlite.org/opcode.html

pub mod builder;
pub mod explain;
pub mod insn;
pub mod likeop;
pub mod sorter;

use crate::error::{LimboError, SQLITE_CONSTRAINT_PRIMARYKEY};
use crate::ext::ExtValue;
use crate::fast_lock::SpinLock;
use crate::function::{AggFunc, ExtFunc, FuncCtx, MathFunc, MathFuncArity, ScalarFunc, VectorFunc};
use crate::functions::datetime::{
    exec_date, exec_datetime_full, exec_julianday, exec_strftime, exec_time, exec_unixepoch,
};
use crate::functions::printf::exec_printf;

use crate::pseudo::PseudoCursor;
use crate::result::LimboResult;
use crate::schema::{affinity, Affinity};
use crate::storage::sqlite3_ondisk::DatabaseHeader;
use crate::storage::wal::CheckpointResult;
use crate::storage::{btree::BTreeCursor, pager::Pager};
use crate::translate::plan::{ResultSetColumn, TableReference};
use crate::types::{
    AggContext, Cursor, CursorResult, ExternalAggState, OwnedValue, Record, SeekKey, SeekOp,
};
use crate::util::{
    cast_real_to_integer, cast_text_to_integer, cast_text_to_numeric, cast_text_to_real,
    checked_cast_text_to_numeric, parse_schema_rows, RoundToPrecision,
};
use crate::vdbe::builder::CursorType;
use crate::vdbe::insn::Insn;
use crate::vector::{vector32, vector64, vector_distance_cos, vector_extract};
use crate::{bail_constraint_error, info, CheckpointStatus};
#[cfg(feature = "json")]
use crate::{
    function::JsonFunc, json::get_json, json::is_json_valid, json::json_array,
    json::json_array_length, json::json_arrow_extract, json::json_arrow_shift_extract,
    json::json_error_position, json::json_extract, json::json_insert, json::json_object,
    json::json_patch, json::json_quote, json::json_remove, json::json_replace, json::json_set,
    json::json_type, json::jsonb, json::jsonb_array, json::jsonb_extract, json::jsonb_insert,
    json::jsonb_object, json::jsonb_remove, json::jsonb_replace, json::JsonCacheCell,
};
use crate::{
    resolve_ext_path, Connection, MvCursor, MvStore, Result, TransactionState, DATABASE_VERSION,
};
use insn::{
    exec_add, exec_and, exec_bit_and, exec_bit_not, exec_bit_or, exec_boolean_not, exec_concat,
    exec_divide, exec_multiply, exec_or, exec_remainder, exec_shift_left, exec_shift_right,
    exec_subtract, Cookie,
};

use likeop::{construct_like_escape_arg, exec_glob, exec_like_with_escape};
use rand::distributions::{Distribution, Uniform};
use rand::{thread_rng, Rng};
use regex::{Regex, RegexBuilder};
use sorter::Sorter;
use std::borrow::BorrowMut;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::num::NonZero;
use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Represents a target for a jump instruction.
/// Stores 32-bit ints to keep the enum word-sized.
pub enum BranchOffset {
    /// A label is a named location in the program.
    /// If there are references to it, it must always be resolved to an Offset
    /// via program.resolve_label().
    Label(u32),
    /// An offset is a direct index into the instruction list.
    Offset(InsnReference),
    /// A placeholder is a temporary value to satisfy the compiler.
    /// It must be set later.
    Placeholder,
}

impl BranchOffset {
    /// Returns true if the branch offset is a label.
    pub fn is_label(&self) -> bool {
        matches!(self, BranchOffset::Label(_))
    }

    /// Returns true if the branch offset is an offset.
    pub fn is_offset(&self) -> bool {
        matches!(self, BranchOffset::Offset(_))
    }

    /// Returns the offset value. Panics if the branch offset is a label or placeholder.
    pub fn to_offset_int(&self) -> InsnReference {
        match self {
            BranchOffset::Label(v) => unreachable!("Unresolved label: {}", v),
            BranchOffset::Offset(v) => *v,
            BranchOffset::Placeholder => unreachable!("Unresolved placeholder"),
        }
    }

    /// Returns the label value. Panics if the branch offset is an offset or placeholder.
    pub fn to_label_value(&self) -> u32 {
        match self {
            BranchOffset::Label(v) => *v,
            BranchOffset::Offset(_) => unreachable!("Offset cannot be converted to label value"),
            BranchOffset::Placeholder => unreachable!("Unresolved placeholder"),
        }
    }

    /// Returns the branch offset as a signed integer.
    /// Used in explain output, where we don't want to panic in case we have an unresolved
    /// label or placeholder.
    pub fn to_debug_int(&self) -> i32 {
        match self {
            BranchOffset::Label(v) => *v as i32,
            BranchOffset::Offset(v) => *v as i32,
            BranchOffset::Placeholder => i32::MAX,
        }
    }

    /// Adds an integer value to the branch offset.
    /// Returns a new branch offset.
    /// Panics if the branch offset is a label or placeholder.
    pub fn add<N: Into<u32>>(self, n: N) -> BranchOffset {
        BranchOffset::Offset(self.to_offset_int() + n.into())
    }
}

pub type CursorID = usize;

pub type PageIdx = usize;

// Index of insn in list of insns
type InsnReference = u32;

#[derive(Debug)]
pub enum StepResult {
    Done,
    IO,
    Row,
    Interrupt,
    Busy,
}

/// If there is I/O, the instruction is restarted.
/// Evaluate a Result<CursorResult<T>>, if IO return Ok(StepResult::IO).
macro_rules! return_if_io {
    ($expr:expr) => {
        match $expr? {
            CursorResult::Ok(v) => v,
            CursorResult::IO => return Ok(StepResult::IO),
        }
    };
}

struct RegexCache {
    like: HashMap<String, Regex>,
    glob: HashMap<String, Regex>,
}

impl RegexCache {
    fn new() -> Self {
        Self {
            like: HashMap::new(),
            glob: HashMap::new(),
        }
    }
}

struct Bitfield<const N: usize>([u64; N]);

impl<const N: usize> Bitfield<N> {
    fn new() -> Self {
        Self([0; N])
    }

    fn set(&mut self, bit: usize) {
        assert!(bit < N * 64, "bit out of bounds");
        self.0[bit / 64] |= 1 << (bit % 64);
    }

    fn unset(&mut self, bit: usize) {
        assert!(bit < N * 64, "bit out of bounds");
        self.0[bit / 64] &= !(1 << (bit % 64));
    }

    fn get(&self, bit: usize) -> bool {
        assert!(bit < N * 64, "bit out of bounds");
        (self.0[bit / 64] & (1 << (bit % 64))) != 0
    }
}

pub struct VTabOpaqueCursor(*const c_void);

impl VTabOpaqueCursor {
    pub fn new(cursor: *const c_void) -> Result<Self> {
        if cursor.is_null() {
            return Err(LimboError::InternalError(
                "VTabOpaqueCursor: cursor is null".into(),
            ));
        }
        Ok(Self(cursor))
    }

    pub fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

#[derive(Copy, Clone)]
enum HaltState {
    Checkpointing,
}

#[derive(Debug, Clone)]
pub enum Register {
    OwnedValue(OwnedValue),
    Aggregate(AggContext),
    Record(Record),
}

/// The program state describes the environment in which the program executes.
pub struct ProgramState {
    pub pc: InsnReference,
    cursors: RefCell<Vec<Option<Cursor>>>,
    registers: Vec<Register>,
    pub(crate) result_row: Option<Record>,
    last_compare: Option<std::cmp::Ordering>,
    deferred_seek: Option<(CursorID, CursorID)>,
    ended_coroutine: Bitfield<4>, // flag to indicate that a coroutine has ended (key is the yield register. currently we assume that the yield register is always between 0-255, YOLO)
    regex_cache: RegexCache,
    pub(crate) mv_tx_id: Option<crate::mvcc::database::TxID>,
    interrupted: bool,
    parameters: HashMap<NonZero<usize>, OwnedValue>,
    halt_state: Option<HaltState>,
    #[cfg(feature = "json")]
    json_cache: JsonCacheCell,
}

impl ProgramState {
    pub fn new(max_registers: usize, max_cursors: usize) -> Self {
        let cursors: RefCell<Vec<Option<Cursor>>> =
            RefCell::new((0..max_cursors).map(|_| None).collect());
        let registers = vec![Register::OwnedValue(OwnedValue::Null); max_registers];
        Self {
            pc: 0,
            cursors,
            registers,
            result_row: None,
            last_compare: None,
            deferred_seek: None,
            ended_coroutine: Bitfield::new(),
            regex_cache: RegexCache::new(),
            mv_tx_id: None,
            interrupted: false,
            parameters: HashMap::new(),
            halt_state: None,
            #[cfg(feature = "json")]
            json_cache: JsonCacheCell::new(),
        }
    }

    pub fn column_count(&self) -> usize {
        self.registers.len()
    }

    pub fn column(&self, i: usize) -> Option<String> {
        Some(format!("{:?}", self.registers[i]))
    }

    pub fn interrupt(&mut self) {
        self.interrupted = true;
    }

    pub fn is_interrupted(&self) -> bool {
        self.interrupted
    }

    pub fn bind_at(&mut self, index: NonZero<usize>, value: OwnedValue) {
        self.parameters.insert(index, value);
    }

    pub fn get_parameter(&self, index: NonZero<usize>) -> Option<&OwnedValue> {
        self.parameters.get(&index)
    }

    pub fn reset(&mut self) {
        self.pc = 0;
        self.cursors.borrow_mut().iter_mut().for_each(|c| *c = None);
        self.registers
            .iter_mut()
            .for_each(|r| *r = Register::OwnedValue(OwnedValue::Null));
        self.last_compare = None;
        self.deferred_seek = None;
        self.ended_coroutine.0 = [0; 4];
        self.regex_cache.like.clear();
        self.interrupted = false;
        self.parameters.clear();
        #[cfg(feature = "json")]
        self.json_cache.clear()
    }

    pub fn get_cursor<'a>(&'a self, cursor_id: CursorID) -> std::cell::RefMut<'a, Cursor> {
        let cursors = self.cursors.borrow_mut();
        std::cell::RefMut::map(cursors, |c| {
            c.get_mut(cursor_id)
                .expect("cursor id out of bounds")
                .as_mut()
                .expect("cursor not allocated")
        })
    }
}

impl Register {
    pub fn get_owned_value(&self) -> &OwnedValue {
        match self {
            Register::OwnedValue(v) => v,
            _ => unreachable!(),
        }
    }
}

macro_rules! must_be_btree_cursor {
    ($cursor_id:expr, $cursor_ref:expr, $state:expr, $insn_name:expr) => {{
        let (_, cursor_type) = $cursor_ref.get($cursor_id).unwrap();
        let cursor = match cursor_type {
            CursorType::BTreeTable(_) => $state.get_cursor($cursor_id),
            CursorType::BTreeIndex(_) => $state.get_cursor($cursor_id),
            CursorType::Pseudo(_) => panic!("{} on pseudo cursor", $insn_name),
            CursorType::Sorter => panic!("{} on sorter cursor", $insn_name),
            CursorType::VirtualTable(_) => panic!("{} on virtual table cursor", $insn_name),
        };
        cursor
    }};
}

#[derive(Debug)]
pub struct Program {
    pub max_registers: usize,
    pub insns: Vec<Insn>,
    pub cursor_ref: Vec<(Option<String>, CursorType)>,
    pub database_header: Arc<SpinLock<DatabaseHeader>>,
    pub comments: Option<HashMap<InsnReference, &'static str>>,
    pub parameters: crate::parameters::Parameters,
    pub connection: Weak<Connection>,
    pub n_change: Cell<i64>,
    pub change_cnt_on: bool,
    pub result_columns: Vec<ResultSetColumn>,
    pub table_references: Vec<TableReference>,
}

impl Program {
    #[rustfmt::skip]
    pub fn explain(&self) -> String {
        let mut buff = String::with_capacity(1024);
        buff.push_str("addr  opcode             p1    p2    p3    p4             p5  comment\n");
        buff.push_str("----  -----------------  ----  ----  ----  -------------  --  -------\n");
        let mut indent_count: usize = 0;
        let indent = "  ";
        let mut prev_insn: Option<&Insn> = None;
        for (addr, insn) in self.insns.iter().enumerate() {
            indent_count = get_indent_count(indent_count, insn, prev_insn);
            print_insn(
                self,
                addr as InsnReference,
                insn,
                indent.repeat(indent_count),
                &mut buff,
            );
            buff.push('\n');
            prev_insn = Some(insn);
        }
        buff
    }

    pub fn step(
        &self,
        state: &mut ProgramState,
        mv_store: Option<Rc<MvStore>>,
        pager: Rc<Pager>,
    ) -> Result<StepResult> {
        loop {
            if state.is_interrupted() {
                return Ok(StepResult::Interrupt);
            }
            let insn = &self.insns[state.pc as usize];
            trace_insn(self, state.pc as InsnReference, insn);
            match insn {
                Insn::Init { target_pc } => {
                    assert!(target_pc.is_offset());
                    state.pc = target_pc.to_offset_int();
                }
                Insn::Add { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_add(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Subtract { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_subtract(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Multiply { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_multiply(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Divide { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_divide(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Remainder { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_remainder(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::BitAnd { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_bit_and(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::BitOr { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_bit_or(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::BitNot { reg, dest } => {
                    state.registers[*dest] =
                        Register::OwnedValue(exec_bit_not(state.registers[*reg].get_owned_value()));
                    state.pc += 1;
                }
                Insn::Checkpoint {
                    database: _,
                    checkpoint_mode: _,
                    dest,
                } => {
                    let result = self.connection.upgrade().unwrap().checkpoint();
                    match result {
                        Ok(CheckpointResult {
                            num_wal_frames: num_wal_pages,
                            num_checkpointed_frames: num_checkpointed_pages,
                        }) => {
                            // https://sqlite.org/pragma.html#pragma_wal_checkpoint
                            // 1st col: 1 (checkpoint SQLITE_BUSY) or 0 (not busy).
                            state.registers[*dest] = Register::OwnedValue(OwnedValue::Integer(0));
                            // 2nd col: # modified pages written to wal file
                            state.registers[*dest + 1] =
                                Register::OwnedValue(OwnedValue::Integer(num_wal_pages as i64));
                            // 3rd col: # pages moved to db after checkpoint
                            state.registers[*dest + 2] = Register::OwnedValue(OwnedValue::Integer(
                                num_checkpointed_pages as i64,
                            ));
                        }
                        Err(_err) => {
                            state.registers[*dest] = Register::OwnedValue(OwnedValue::Integer(1))
                        }
                    }

                    state.pc += 1;
                }
                Insn::Null { dest, dest_end } => {
                    if let Some(dest_end) = dest_end {
                        for i in *dest..=*dest_end {
                            state.registers[i] = Register::OwnedValue(OwnedValue::Null);
                        }
                    } else {
                        state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                    }
                    state.pc += 1;
                }
                Insn::NullRow { cursor_id } => {
                    {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor_id, self.cursor_ref, state, "NullRow");
                        let cursor = cursor.as_btree_mut();
                        cursor.set_null_flag(true);
                    }
                    state.pc += 1;
                }
                Insn::Compare {
                    start_reg_a,
                    start_reg_b,
                    count,
                } => {
                    let start_reg_a = *start_reg_a;
                    let start_reg_b = *start_reg_b;
                    let count = *count;

                    if start_reg_a + count > start_reg_b {
                        return Err(LimboError::InternalError(
                            "Compare registers overlap".to_string(),
                        ));
                    }

                    let mut cmp = None;
                    for i in 0..count {
                        let a = state.registers[start_reg_a + i].get_owned_value();
                        let b = state.registers[start_reg_b + i].get_owned_value();
                        cmp = Some(a.cmp(b));
                        if cmp != Some(std::cmp::Ordering::Equal) {
                            break;
                        }
                    }
                    state.last_compare = cmp;
                    state.pc += 1;
                }
                Insn::Jump {
                    target_pc_lt,
                    target_pc_eq,
                    target_pc_gt,
                } => {
                    assert!(target_pc_lt.is_offset());
                    assert!(target_pc_eq.is_offset());
                    assert!(target_pc_gt.is_offset());
                    let cmp = state.last_compare.take();
                    if cmp.is_none() {
                        return Err(LimboError::InternalError(
                            "Jump without compare".to_string(),
                        ));
                    }
                    let target_pc = match cmp.unwrap() {
                        std::cmp::Ordering::Less => *target_pc_lt,
                        std::cmp::Ordering::Equal => *target_pc_eq,
                        std::cmp::Ordering::Greater => *target_pc_gt,
                    };
                    state.pc = target_pc.to_offset_int();
                }
                Insn::Move {
                    source_reg,
                    dest_reg,
                    count,
                } => {
                    let source_reg = *source_reg;
                    let dest_reg = *dest_reg;
                    let count = *count;
                    for i in 0..count {
                        state.registers[dest_reg + i] = std::mem::replace(
                            &mut state.registers[source_reg + i],
                            Register::OwnedValue(OwnedValue::Null),
                        );
                    }
                    state.pc += 1;
                }
                Insn::IfPos {
                    reg,
                    target_pc,
                    decrement_by,
                } => {
                    assert!(target_pc.is_offset());
                    let reg = *reg;
                    let target_pc = *target_pc;
                    match state.registers[reg].get_owned_value() {
                        OwnedValue::Integer(n) if *n > 0 => {
                            state.pc = target_pc.to_offset_int();
                            state.registers[reg] = Register::OwnedValue(OwnedValue::Integer(
                                *n - *decrement_by as i64,
                            ));
                        }
                        OwnedValue::Integer(_) => {
                            state.pc += 1;
                        }
                        _ => {
                            return Err(LimboError::InternalError(
                                "IfPos: the value in the register is not an integer".into(),
                            ));
                        }
                    }
                }
                Insn::NotNull { reg, target_pc } => {
                    assert!(target_pc.is_offset());
                    let reg = *reg;
                    let target_pc = *target_pc;
                    match &state.registers[reg].get_owned_value() {
                        OwnedValue::Null => {
                            state.pc += 1;
                        }
                        _ => {
                            state.pc = target_pc.to_offset_int();
                        }
                    }
                }

                Insn::Eq {
                    lhs,
                    rhs,
                    target_pc,
                    flags,
                } => {
                    assert!(target_pc.is_offset());
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    let cond = *state.registers[lhs].get_owned_value()
                        == *state.registers[rhs].get_owned_value();
                    let nulleq = flags.has_nulleq();
                    let jump_if_null = flags.has_jump_if_null();
                    match (
                        &state.registers[lhs].get_owned_value(),
                        &state.registers[rhs].get_owned_value(),
                    ) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            if (nulleq && cond) || (!nulleq && jump_if_null) {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                        _ => {
                            if *state.registers[lhs].get_owned_value()
                                == *state.registers[rhs].get_owned_value()
                            {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Ne {
                    lhs,
                    rhs,
                    target_pc,
                    flags,
                } => {
                    assert!(target_pc.is_offset());
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    let cond = *state.registers[lhs].get_owned_value()
                        != *state.registers[rhs].get_owned_value();
                    let nulleq = flags.has_nulleq();
                    let jump_if_null = flags.has_jump_if_null();
                    match (
                        &state.registers[lhs].get_owned_value(),
                        &state.registers[rhs].get_owned_value(),
                    ) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            if (nulleq && cond) || (!nulleq && jump_if_null) {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                        _ => {
                            if *state.registers[lhs].get_owned_value()
                                != *state.registers[rhs].get_owned_value()
                            {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Lt {
                    lhs,
                    rhs,
                    target_pc,
                    flags,
                } => {
                    assert!(target_pc.is_offset());
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    let jump_if_null = flags.has_jump_if_null();
                    match (
                        &state.registers[lhs].get_owned_value(),
                        &state.registers[rhs].get_owned_value(),
                    ) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            if jump_if_null {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                        _ => {
                            if *state.registers[lhs].get_owned_value()
                                < *state.registers[rhs].get_owned_value()
                            {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Le {
                    lhs,
                    rhs,
                    target_pc,
                    flags,
                } => {
                    assert!(target_pc.is_offset());
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    let jump_if_null = flags.has_jump_if_null();
                    match (
                        &state.registers[lhs].get_owned_value(),
                        &state.registers[rhs].get_owned_value(),
                    ) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            if jump_if_null {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                        _ => {
                            if *state.registers[lhs].get_owned_value()
                                <= *state.registers[rhs].get_owned_value()
                            {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Gt {
                    lhs,
                    rhs,
                    target_pc,
                    flags,
                } => {
                    assert!(target_pc.is_offset());
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    let jump_if_null = flags.has_jump_if_null();
                    match (
                        &state.registers[lhs].get_owned_value(),
                        &state.registers[rhs].get_owned_value(),
                    ) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            if jump_if_null {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                        _ => {
                            if *state.registers[lhs].get_owned_value()
                                > *state.registers[rhs].get_owned_value()
                            {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::Ge {
                    lhs,
                    rhs,
                    target_pc,
                    flags,
                } => {
                    assert!(target_pc.is_offset());
                    let lhs = *lhs;
                    let rhs = *rhs;
                    let target_pc = *target_pc;
                    let jump_if_null = flags.has_jump_if_null();
                    match (
                        &state.registers[lhs].get_owned_value(),
                        &state.registers[rhs].get_owned_value(),
                    ) {
                        (_, OwnedValue::Null) | (OwnedValue::Null, _) => {
                            if jump_if_null {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                        _ => {
                            if *state.registers[lhs].get_owned_value()
                                >= *state.registers[rhs].get_owned_value()
                            {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.pc += 1;
                            }
                        }
                    }
                }
                Insn::If {
                    reg,
                    target_pc,
                    jump_if_null,
                } => {
                    assert!(target_pc.is_offset());
                    if exec_if(
                        &state.registers[*reg].get_owned_value(),
                        *jump_if_null,
                        false,
                    ) {
                        state.pc = target_pc.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::IfNot {
                    reg,
                    target_pc,
                    jump_if_null,
                } => {
                    assert!(target_pc.is_offset());
                    if exec_if(
                        &state.registers[*reg].get_owned_value(),
                        *jump_if_null,
                        true,
                    ) {
                        state.pc = target_pc.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::OpenReadAsync {
                    cursor_id,
                    root_page,
                } => {
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    let mv_cursor = match state.mv_tx_id {
                        Some(tx_id) => {
                            let table_id = *root_page as u64;
                            let mv_store = mv_store.as_ref().unwrap().clone();
                            let mv_cursor = Rc::new(RefCell::new(
                                MvCursor::new(mv_store, tx_id, table_id).unwrap(),
                            ));
                            Some(mv_cursor)
                        }
                        None => None,
                    };
                    let cursor = BTreeCursor::new(mv_cursor, pager.clone(), *root_page);
                    let mut cursors = state.cursors.borrow_mut();
                    match cursor_type {
                        CursorType::BTreeTable(_) => {
                            cursors
                                .get_mut(*cursor_id)
                                .unwrap()
                                .replace(Cursor::new_btree(cursor));
                        }
                        CursorType::BTreeIndex(_) => {
                            cursors
                                .get_mut(*cursor_id)
                                .unwrap()
                                .replace(Cursor::new_btree(cursor));
                        }
                        CursorType::Pseudo(_) => {
                            panic!("OpenReadAsync on pseudo cursor");
                        }
                        CursorType::Sorter => {
                            panic!("OpenReadAsync on sorter cursor");
                        }
                        CursorType::VirtualTable(_) => {
                            panic!("OpenReadAsync on virtual table cursor, use Insn::VOpenAsync instead");
                        }
                    }
                    state.pc += 1;
                }
                Insn::OpenReadAwait => {
                    state.pc += 1;
                }
                Insn::VOpenAsync { cursor_id } => {
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    let CursorType::VirtualTable(virtual_table) = cursor_type else {
                        panic!("VOpenAsync on non-virtual table cursor");
                    };
                    let cursor = virtual_table.open()?;
                    state
                        .cursors
                        .borrow_mut()
                        .insert(*cursor_id, Some(Cursor::Virtual(cursor)));
                    state.pc += 1;
                }
                Insn::VCreate {
                    module_name,
                    table_name,
                    args_reg,
                } => {
                    let module_name = state.registers[*module_name].get_owned_value().to_string();
                    let table_name = state.registers[*table_name].get_owned_value().to_string();
                    let args = if let Some(args_reg) = args_reg {
                        if let Register::Record(rec) = &state.registers[*args_reg] {
                            rec.get_values().iter().map(|v| v.to_ffi()).collect()
                        } else {
                            return Err(LimboError::InternalError(
                                "VCreate: args_reg is not a record".to_string(),
                            ));
                        }
                    } else {
                        vec![]
                    };
                    let Some(conn) = self.connection.upgrade() else {
                        return Err(crate::LimboError::ExtensionError(
                            "Failed to upgrade Connection".to_string(),
                        ));
                    };
                    let table = crate::VirtualTable::from_args(
                        Some(&table_name),
                        &module_name,
                        args,
                        &conn.syms.borrow(),
                        limbo_ext::VTabKind::VirtualTable,
                        None,
                    )?;
                    {
                        conn.syms
                            .borrow_mut()
                            .vtabs
                            .insert(table_name, table.clone());
                    }
                    state.pc += 1;
                }
                Insn::VOpenAwait => {
                    state.pc += 1;
                }
                Insn::VFilter {
                    cursor_id,
                    pc_if_empty,
                    arg_count,
                    args_reg,
                } => {
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    let CursorType::VirtualTable(virtual_table) = cursor_type else {
                        panic!("VFilter on non-virtual table cursor");
                    };
                    let has_rows = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_virtual_mut();
                        let mut args = Vec::new();
                        for i in 0..*arg_count {
                            args.push(state.registers[args_reg + i].get_owned_value().clone());
                        }
                        virtual_table.filter(cursor, *arg_count, args)?
                    };
                    if !has_rows {
                        state.pc = pc_if_empty.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::VColumn {
                    cursor_id,
                    column,
                    dest,
                } => {
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    let CursorType::VirtualTable(virtual_table) = cursor_type else {
                        panic!("VColumn on non-virtual table cursor");
                    };
                    let value = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_virtual_mut();
                        virtual_table.column(cursor, *column)?
                    };
                    state.registers[*dest] = Register::OwnedValue(value);
                    state.pc += 1;
                }
                Insn::VUpdate {
                    cursor_id,
                    arg_count,
                    start_reg,
                    conflict_action,
                    ..
                } => {
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    let CursorType::VirtualTable(virtual_table) = cursor_type else {
                        panic!("VUpdate on non-virtual table cursor");
                    };

                    if *arg_count < 2 {
                        return Err(LimboError::InternalError(
                            "VUpdate: arg_count must be at least 2 (rowid and insert_rowid)"
                                .to_string(),
                        ));
                    }
                    let mut argv = Vec::with_capacity(*arg_count);
                    for i in 0..*arg_count {
                        if let Some(value) = state.registers.get(*start_reg + i) {
                            argv.push(value.get_owned_value().clone());
                        } else {
                            return Err(LimboError::InternalError(format!(
                                "VUpdate: register out of bounds at {}",
                                *start_reg + i
                            )));
                        }
                    }
                    let result = virtual_table.update(&argv);
                    match result {
                        Ok(Some(new_rowid)) => {
                            if *conflict_action == 5 {
                                // ResolveType::Replace
                                if let Some(conn) = self.connection.upgrade() {
                                    conn.update_last_rowid(new_rowid as u64);
                                }
                            }
                            state.pc += 1;
                        }
                        Ok(None) => {
                            // no-op or successful update without rowid return
                            state.pc += 1;
                        }
                        Err(e) => {
                            // virtual table update failed
                            return Err(LimboError::ExtensionError(format!(
                                "Virtual table update failed: {}",
                                e
                            )));
                        }
                    }
                }
                Insn::VNext {
                    cursor_id,
                    pc_if_next,
                } => {
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    let CursorType::VirtualTable(virtual_table) = cursor_type else {
                        panic!("VNextAsync on non-virtual table cursor");
                    };
                    let has_more = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_virtual_mut();
                        virtual_table.next(cursor)?
                    };
                    if has_more {
                        state.pc = pc_if_next.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::OpenPseudo {
                    cursor_id,
                    content_reg: _,
                    num_fields: _,
                } => {
                    {
                        let mut cursors = state.cursors.borrow_mut();
                        let cursor = PseudoCursor::new();
                        cursors
                            .get_mut(*cursor_id)
                            .unwrap()
                            .replace(Cursor::new_pseudo(cursor));
                    }
                    state.pc += 1;
                }
                Insn::RewindAsync { cursor_id } => {
                    {
                        let mut cursor = must_be_btree_cursor!(
                            *cursor_id,
                            self.cursor_ref,
                            state,
                            "RewindAsync"
                        );
                        let cursor = cursor.as_btree_mut();
                        return_if_io!(cursor.rewind());
                    }
                    state.pc += 1;
                }
                Insn::LastAsync { cursor_id } => {
                    {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor_id, self.cursor_ref, state, "LastAsync");
                        let cursor = cursor.as_btree_mut();
                        return_if_io!(cursor.last());
                    }
                    state.pc += 1;
                }
                Insn::LastAwait {
                    cursor_id,
                    pc_if_empty,
                } => {
                    assert!(pc_if_empty.is_offset());
                    let is_empty = {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor_id, self.cursor_ref, state, "LastAwait");
                        let cursor = cursor.as_btree_mut();
                        cursor.wait_for_completion()?;
                        cursor.is_empty()
                    };
                    if is_empty {
                        state.pc = pc_if_empty.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::RewindAwait {
                    cursor_id,
                    pc_if_empty,
                } => {
                    assert!(pc_if_empty.is_offset());
                    let is_empty = {
                        let mut cursor = must_be_btree_cursor!(
                            *cursor_id,
                            self.cursor_ref,
                            state,
                            "RewindAwait"
                        );
                        let cursor = cursor.as_btree_mut();
                        cursor.wait_for_completion()?;
                        cursor.is_empty()
                    };
                    if is_empty {
                        state.pc = pc_if_empty.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::Column {
                    cursor_id,
                    column,
                    dest,
                } => {
                    if let Some((index_cursor_id, table_cursor_id)) = state.deferred_seek.take() {
                        let deferred_seek = {
                            let rowid = {
                                let mut index_cursor = state.get_cursor(index_cursor_id);
                                let index_cursor = index_cursor.as_btree_mut();
                                index_cursor.rowid()?
                            };
                            let mut table_cursor = state.get_cursor(table_cursor_id);
                            let table_cursor = table_cursor.as_btree_mut();
                            match table_cursor
                                .seek(SeekKey::TableRowId(rowid.unwrap()), SeekOp::EQ)?
                            {
                                CursorResult::Ok(_) => None,
                                CursorResult::IO => Some((index_cursor_id, table_cursor_id)),
                            }
                        };
                        if let Some(deferred_seek) = deferred_seek {
                            state.deferred_seek = Some(deferred_seek);
                            return Ok(StepResult::IO);
                        }
                    }
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    match cursor_type {
                        CursorType::BTreeTable(_) | CursorType::BTreeIndex(_) => {
                            let value = {
                                let mut cursor = must_be_btree_cursor!(
                                    *cursor_id,
                                    self.cursor_ref,
                                    state,
                                    "Column"
                                );
                                let cursor = cursor.as_btree_mut();
                                let record = cursor.record();
                                if let Some(record) = record.as_ref() {
                                    if cursor.get_null_flag() {
                                        OwnedValue::Null
                                    } else {
                                        record.get_value(*column).to_owned()
                                    }
                                } else {
                                    OwnedValue::Null
                                }
                            };
                            state.registers[*dest] = Register::OwnedValue(value);
                        }
                        CursorType::Sorter => {
                            let record = {
                                let mut cursor = state.get_cursor(*cursor_id);
                                let cursor = cursor.as_sorter_mut();
                                cursor.record().map(|r| r.clone())
                            };
                            if let Some(record) = record {
                                state.registers[*dest] =
                                    Register::OwnedValue(record.get_value(*column).clone());
                            } else {
                                state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                            }
                        }
                        CursorType::Pseudo(_) => {
                            let value = {
                                let mut cursor = state.get_cursor(*cursor_id);
                                let cursor = cursor.as_pseudo_mut();
                                if let Some(record) = cursor.record() {
                                    record.get_value(*column).clone()
                                } else {
                                    OwnedValue::Null
                                }
                            };
                            state.registers[*dest] = Register::OwnedValue(value);
                        }
                        CursorType::VirtualTable(_) => {
                            panic!(
                                "Insn::Column on virtual table cursor, use Insn::VColumn instead"
                            );
                        }
                    }

                    state.pc += 1;
                }
                Insn::MakeRecord {
                    start_reg,
                    count,
                    dest_reg,
                } => {
                    let record = make_owned_record(&state.registers, start_reg, count);
                    state.registers[*dest_reg] = Register::Record(record);
                    state.pc += 1;
                }
                Insn::ResultRow { start_reg, count } => {
                    let record = make_owned_record(&state.registers, start_reg, count);
                    state.result_row = Some(record);
                    state.pc += 1;
                    return Ok(StepResult::Row);
                }
                Insn::NextAsync { cursor_id } => {
                    {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor_id, self.cursor_ref, state, "NextAsync");
                        let cursor = cursor.as_btree_mut();
                        cursor.set_null_flag(false);
                        return_if_io!(cursor.next());
                    }
                    state.pc += 1;
                }
                Insn::PrevAsync { cursor_id } => {
                    {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor_id, self.cursor_ref, state, "PrevAsync");
                        let cursor = cursor.as_btree_mut();
                        cursor.set_null_flag(false);
                        return_if_io!(cursor.prev());
                    }
                    state.pc += 1;
                }
                Insn::PrevAwait {
                    cursor_id,
                    pc_if_next,
                } => {
                    assert!(pc_if_next.is_offset());
                    let is_empty = {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor_id, self.cursor_ref, state, "PrevAwait");
                        let cursor = cursor.as_btree_mut();
                        cursor.wait_for_completion()?;
                        cursor.is_empty()
                    };
                    if !is_empty {
                        state.pc = pc_if_next.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::NextAwait {
                    cursor_id,
                    pc_if_next,
                } => {
                    assert!(pc_if_next.is_offset());
                    let is_empty = {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor_id, self.cursor_ref, state, "NextAwait");
                        let cursor = cursor.as_btree_mut();
                        cursor.wait_for_completion()?;
                        cursor.is_empty()
                    };
                    if !is_empty {
                        state.pc = pc_if_next.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::Halt {
                    err_code,
                    description,
                } => {
                    match *err_code {
                        0 => {}
                        SQLITE_CONSTRAINT_PRIMARYKEY => {
                            return Err(LimboError::Constraint(format!(
                                "UNIQUE constraint failed: {} (19)",
                                description
                            )));
                        }
                        _ => {
                            return Err(LimboError::Constraint(format!(
                                "undocumented halt error code {}",
                                description
                            )));
                        }
                    }
                    return self.halt(pager, state, mv_store);
                }
                Insn::Transaction { write } => {
                    if let Some(mv_store) = &mv_store {
                        if state.mv_tx_id.is_none() {
                            let tx_id = mv_store.begin_tx();
                            self.connection
                                .upgrade()
                                .unwrap()
                                .mv_transactions
                                .borrow_mut()
                                .push(tx_id);
                            state.mv_tx_id = Some(tx_id);
                        }
                    } else {
                        let connection = self.connection.upgrade().unwrap();
                        let current_state = connection.transaction_state.borrow().clone();
                        let (new_transaction_state, updated) = match (&current_state, write) {
                            (TransactionState::Write, true) => (TransactionState::Write, false),
                            (TransactionState::Write, false) => (TransactionState::Write, false),
                            (TransactionState::Read, true) => (TransactionState::Write, true),
                            (TransactionState::Read, false) => (TransactionState::Read, false),
                            (TransactionState::None, true) => (TransactionState::Write, true),
                            (TransactionState::None, false) => (TransactionState::Read, true),
                        };

                        if updated && matches!(current_state, TransactionState::None) {
                            if let LimboResult::Busy = pager.begin_read_tx()? {
                                return Ok(StepResult::Busy);
                            }
                        }

                        if updated && matches!(new_transaction_state, TransactionState::Write) {
                            if let LimboResult::Busy = pager.begin_write_tx()? {
                                tracing::trace!("begin_write_tx busy");
                                return Ok(StepResult::Busy);
                            }
                        }
                        if updated {
                            connection.transaction_state.replace(new_transaction_state);
                        }
                    }
                    state.pc += 1;
                }
                Insn::AutoCommit {
                    auto_commit,
                    rollback,
                } => {
                    let conn = self.connection.upgrade().unwrap();
                    if matches!(state.halt_state, Some(HaltState::Checkpointing)) {
                        return self.halt(pager, state, mv_store);
                    }

                    if *auto_commit != *conn.auto_commit.borrow() {
                        if *rollback {
                            todo!("Rollback is not implemented");
                        } else {
                            conn.auto_commit.replace(*auto_commit);
                        }
                    } else if !*auto_commit {
                        return Err(LimboError::TxError(
                            "cannot start a transaction within a transaction".to_string(),
                        ));
                    } else if *rollback {
                        return Err(LimboError::TxError(
                            "cannot rollback - no transaction is active".to_string(),
                        ));
                    } else {
                        return Err(LimboError::TxError(
                            "cannot commit - no transaction is active".to_string(),
                        ));
                    }
                    return self.halt(pager, state, mv_store);
                }
                Insn::Goto { target_pc } => {
                    assert!(target_pc.is_offset());
                    state.pc = target_pc.to_offset_int();
                }
                Insn::Gosub {
                    target_pc,
                    return_reg,
                } => {
                    assert!(target_pc.is_offset());
                    state.registers[*return_reg] =
                        Register::OwnedValue(OwnedValue::Integer((state.pc + 1) as i64));
                    state.pc = target_pc.to_offset_int();
                }
                Insn::Return { return_reg } => {
                    if let OwnedValue::Integer(pc) = state.registers[*return_reg].get_owned_value()
                    {
                        let pc: u32 = (*pc)
                            .try_into()
                            .unwrap_or_else(|_| panic!("Return register is negative: {}", pc));
                        state.pc = pc;
                    } else {
                        return Err(LimboError::InternalError(
                            "Return register is not an integer".to_string(),
                        ));
                    }
                }
                Insn::Integer { value, dest } => {
                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Integer(*value));
                    state.pc += 1;
                }
                Insn::Real { value, dest } => {
                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Float(*value));
                    state.pc += 1;
                }
                Insn::RealAffinity { register } => {
                    if let OwnedValue::Integer(i) = &state.registers[*register].get_owned_value() {
                        state.registers[*register] =
                            Register::OwnedValue(OwnedValue::Float(*i as f64));
                    };
                    state.pc += 1;
                }
                Insn::String8 { value, dest } => {
                    state.registers[*dest] = Register::OwnedValue(OwnedValue::build_text(value));
                    state.pc += 1;
                }
                Insn::Blob { value, dest } => {
                    state.registers[*dest] =
                        Register::OwnedValue(OwnedValue::Blob(Rc::new(value.clone())));
                    state.pc += 1;
                }
                Insn::RowId { cursor_id, dest } => {
                    if let Some((index_cursor_id, table_cursor_id)) = state.deferred_seek.take() {
                        let deferred_seek = {
                            let rowid = {
                                let mut index_cursor = state.get_cursor(index_cursor_id);
                                let index_cursor = index_cursor.as_btree_mut();
                                let rowid = index_cursor.rowid()?;
                                rowid
                            };
                            let mut table_cursor = state.get_cursor(table_cursor_id);
                            let table_cursor = table_cursor.as_btree_mut();
                            let deferred_seek = match table_cursor
                                .seek(SeekKey::TableRowId(rowid.unwrap()), SeekOp::EQ)?
                            {
                                CursorResult::Ok(_) => None,
                                CursorResult::IO => Some((index_cursor_id, table_cursor_id)),
                            };
                            deferred_seek
                        };
                        if let Some(deferred_seek) = deferred_seek {
                            state.deferred_seek = Some(deferred_seek);
                            return Ok(StepResult::IO);
                        }
                    }
                    let mut cursors = state.cursors.borrow_mut();
                    if let Some(Cursor::BTree(btree_cursor)) = cursors.get_mut(*cursor_id).unwrap()
                    {
                        if let Some(ref rowid) = btree_cursor.rowid()? {
                            state.registers[*dest] =
                                Register::OwnedValue(OwnedValue::Integer(*rowid as i64));
                        } else {
                            state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                        }
                    } else if let Some(Cursor::Virtual(virtual_cursor)) =
                        cursors.get_mut(*cursor_id).unwrap()
                    {
                        let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                        let CursorType::VirtualTable(virtual_table) = cursor_type else {
                            panic!("VUpdate on non-virtual table cursor");
                        };
                        let rowid = virtual_table.rowid(virtual_cursor);
                        if rowid != 0 {
                            state.registers[*dest] =
                                Register::OwnedValue(OwnedValue::Integer(rowid));
                        } else {
                            state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                        }
                    } else {
                        return Err(LimboError::InternalError(
                            "RowId: cursor is not a table or virtual cursor".to_string(),
                        ));
                    }
                    state.pc += 1;
                }
                Insn::SeekRowid {
                    cursor_id,
                    src_reg,
                    target_pc,
                } => {
                    assert!(target_pc.is_offset());
                    let pc = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        let rowid = match state.registers[*src_reg].get_owned_value() {
                            OwnedValue::Integer(rowid) => Some(*rowid as u64),
                            OwnedValue::Null => None,
                            other => {
                                return Err(LimboError::InternalError(
                                    format!("SeekRowid: the value in the register is not an integer or NULL: {}", other)
                                ));
                            }
                        };
                        match rowid {
                            Some(rowid) => {
                                let found = return_if_io!(
                                    cursor.seek(SeekKey::TableRowId(rowid), SeekOp::EQ)
                                );
                                if !found {
                                    target_pc.to_offset_int()
                                } else {
                                    state.pc + 1
                                }
                            }
                            None => target_pc.to_offset_int(),
                        }
                    };
                    state.pc = pc;
                }
                Insn::DeferredSeek {
                    index_cursor_id,
                    table_cursor_id,
                } => {
                    state.deferred_seek = Some((*index_cursor_id, *table_cursor_id));
                    state.pc += 1;
                }
                Insn::SeekGE {
                    cursor_id,
                    start_reg,
                    num_regs,
                    target_pc,
                    is_index,
                } => {
                    assert!(target_pc.is_offset());
                    if *is_index {
                        let found = {
                            let mut cursor = state.get_cursor(*cursor_id);
                            let cursor = cursor.as_btree_mut();
                            let record_from_regs =
                                make_owned_record(&state.registers, start_reg, num_regs);
                            let found = return_if_io!(
                                cursor.seek(SeekKey::IndexKey(&record_from_regs), SeekOp::GE)
                            );
                            found
                        };
                        if !found {
                            state.pc = target_pc.to_offset_int();
                        } else {
                            state.pc += 1;
                        }
                    } else {
                        let pc = {
                            let mut cursor = state.get_cursor(*cursor_id);
                            let cursor = cursor.as_btree_mut();
                            let rowid = match state.registers[*start_reg].get_owned_value() {
                                OwnedValue::Null => {
                                    // All integer values are greater than null so we just rewind the cursor
                                    return_if_io!(cursor.rewind());
                                    None
                                }
                                OwnedValue::Integer(rowid) => Some(*rowid as u64),
                                _ => {
                                    return Err(LimboError::InternalError(
                                        "SeekGE: the value in the register is not an integer"
                                            .into(),
                                    ));
                                }
                            };
                            match rowid {
                                Some(rowid) => {
                                    let found = return_if_io!(
                                        cursor.seek(SeekKey::TableRowId(rowid), SeekOp::GE)
                                    );
                                    if !found {
                                        target_pc.to_offset_int()
                                    } else {
                                        state.pc + 1
                                    }
                                }
                                None => state.pc + 1,
                            }
                        };
                        state.pc = pc;
                    }
                }
                Insn::SeekGT {
                    cursor_id,
                    start_reg,
                    num_regs,
                    target_pc,
                    is_index,
                } => {
                    assert!(target_pc.is_offset());
                    if *is_index {
                        let found = {
                            let mut cursor = state.get_cursor(*cursor_id);
                            let cursor = cursor.as_btree_mut();
                            let record_from_regs: Record =
                                make_owned_record(&state.registers, start_reg, num_regs);
                            let found = return_if_io!(
                                cursor.seek(SeekKey::IndexKey(&record_from_regs), SeekOp::GT)
                            );
                            found
                        };
                        if !found {
                            state.pc = target_pc.to_offset_int();
                        } else {
                            state.pc += 1;
                        }
                    } else {
                        let pc = {
                            let mut cursor = state.get_cursor(*cursor_id);
                            let cursor = cursor.as_btree_mut();
                            let rowid = match state.registers[*start_reg].get_owned_value() {
                                OwnedValue::Null => {
                                    // All integer values are greater than null so we just rewind the cursor
                                    return_if_io!(cursor.rewind());
                                    None
                                }
                                OwnedValue::Integer(rowid) => Some(*rowid as u64),
                                _ => {
                                    return Err(LimboError::InternalError(
                                        "SeekGT: the value in the register is not an integer"
                                            .into(),
                                    ));
                                }
                            };
                            let found = match rowid {
                                Some(rowid) => {
                                    let found = return_if_io!(
                                        cursor.seek(SeekKey::TableRowId(rowid), SeekOp::GT)
                                    );
                                    if !found {
                                        target_pc.to_offset_int()
                                    } else {
                                        state.pc + 1
                                    }
                                }
                                None => state.pc + 1,
                            };
                            found
                        };
                        state.pc = pc;
                    }
                }
                Insn::IdxGE {
                    cursor_id,
                    start_reg,
                    num_regs,
                    target_pc,
                } => {
                    assert!(target_pc.is_offset());
                    let pc = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        let record_from_regs: Record =
                            make_owned_record(&state.registers, start_reg, num_regs);
                        let pc = if let Some(ref idx_record) = *cursor.record() {
                            // Compare against the same number of values
                            if idx_record.get_values()[..record_from_regs.len()]
                                .iter()
                                .zip(&record_from_regs.get_values()[..])
                                .all(|(a, b)| a >= b)
                            {
                                target_pc.to_offset_int()
                            } else {
                                state.pc + 1
                            }
                        } else {
                            target_pc.to_offset_int()
                        };
                        pc
                    };
                    state.pc = pc;
                }
                Insn::IdxLE {
                    cursor_id,
                    start_reg,
                    num_regs,
                    target_pc,
                } => {
                    assert!(target_pc.is_offset());
                    let pc = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        let record_from_regs: Record =
                            make_owned_record(&state.registers, start_reg, num_regs);
                        let pc = if let Some(ref idx_record) = *cursor.record() {
                            // Compare against the same number of values
                            if idx_record.get_values()[..record_from_regs.len()]
                                .iter()
                                .zip(&record_from_regs.get_values()[..])
                                .all(|(a, b)| a <= b)
                            {
                                target_pc.to_offset_int()
                            } else {
                                state.pc + 1
                            }
                        } else {
                            target_pc.to_offset_int()
                        };
                        pc
                    };
                    state.pc = pc;
                }
                Insn::IdxGT {
                    cursor_id,
                    start_reg,
                    num_regs,
                    target_pc,
                } => {
                    assert!(target_pc.is_offset());
                    let pc = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        let record_from_regs: Record =
                            make_owned_record(&state.registers, start_reg, num_regs);
                        let pc = if let Some(ref idx_record) = *cursor.record() {
                            // Compare against the same number of values
                            if idx_record.get_values()[..record_from_regs.len()]
                                .iter()
                                .zip(&record_from_regs.get_values()[..])
                                .all(|(a, b)| a > b)
                            {
                                target_pc.to_offset_int()
                            } else {
                                state.pc + 1
                            }
                        } else {
                            target_pc.to_offset_int()
                        };
                        pc
                    };
                    state.pc = pc;
                }
                Insn::IdxLT {
                    cursor_id,
                    start_reg,
                    num_regs,
                    target_pc,
                } => {
                    assert!(target_pc.is_offset());
                    let pc = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        let record_from_regs: Record =
                            make_owned_record(&state.registers, start_reg, num_regs);
                        let pc = if let Some(ref idx_record) = *cursor.record() {
                            // Compare against the same number of values
                            if idx_record.get_values()[..record_from_regs.len()]
                                .iter()
                                .zip(&record_from_regs.get_values()[..])
                                .all(|(a, b)| a < b)
                            {
                                target_pc.to_offset_int()
                            } else {
                                state.pc + 1
                            }
                        } else {
                            target_pc.to_offset_int()
                        };
                        pc
                    };
                    state.pc = pc;
                }
                Insn::DecrJumpZero { reg, target_pc } => {
                    assert!(target_pc.is_offset());
                    match state.registers[*reg].get_owned_value() {
                        OwnedValue::Integer(n) => {
                            let n = n - 1;
                            if n == 0 {
                                state.pc = target_pc.to_offset_int();
                            } else {
                                state.registers[*reg] =
                                    Register::OwnedValue(OwnedValue::Integer(n));
                                state.pc += 1;
                            }
                        }
                        _ => unreachable!("DecrJumpZero on non-integer register"),
                    }
                }
                Insn::AggStep {
                    acc_reg,
                    col,
                    delimiter,
                    func,
                } => {
                    if let Register::OwnedValue(OwnedValue::Null) = state.registers[*acc_reg] {
                        state.registers[*acc_reg] = match func {
                            AggFunc::Avg => Register::Aggregate(AggContext::Avg(
                                OwnedValue::Float(0.0),
                                OwnedValue::Integer(0),
                            )),
                            AggFunc::Sum => Register::Aggregate(AggContext::Sum(OwnedValue::Null)),
                            AggFunc::Total => {
                                // The result of total() is always a floating point value.
                                // No overflow error is ever raised if any prior input was a floating point value.
                                // Total() never throws an integer overflow.
                                Register::Aggregate(AggContext::Sum(OwnedValue::Float(0.0)))
                            }
                            AggFunc::Count | AggFunc::Count0 => {
                                Register::Aggregate(AggContext::Count(OwnedValue::Integer(0)))
                            }
                            AggFunc::Max => {
                                let col = state.registers[*col].get_owned_value();
                                match col {
                                    OwnedValue::Integer(_) => {
                                        Register::Aggregate(AggContext::Max(None))
                                    }
                                    OwnedValue::Float(_) => {
                                        Register::Aggregate(AggContext::Max(None))
                                    }
                                    OwnedValue::Text(_) => {
                                        Register::Aggregate(AggContext::Max(None))
                                    }
                                    _ => {
                                        unreachable!();
                                    }
                                }
                            }
                            AggFunc::Min => {
                                let col = state.registers[*col].get_owned_value();
                                match col {
                                    OwnedValue::Integer(_) => {
                                        Register::Aggregate(AggContext::Min(None))
                                    }
                                    OwnedValue::Float(_) => {
                                        Register::Aggregate(AggContext::Min(None))
                                    }
                                    OwnedValue::Text(_) => {
                                        Register::Aggregate(AggContext::Min(None))
                                    }
                                    _ => {
                                        unreachable!();
                                    }
                                }
                            }
                            AggFunc::GroupConcat | AggFunc::StringAgg => Register::Aggregate(
                                AggContext::GroupConcat(OwnedValue::build_text("")),
                            ),
                            AggFunc::External(func) => match func.as_ref() {
                                ExtFunc::Aggregate {
                                    init,
                                    step,
                                    finalize,
                                    argc,
                                } => Register::Aggregate(AggContext::External(ExternalAggState {
                                    state: unsafe { (init)() },
                                    argc: *argc,
                                    step_fn: *step,
                                    finalize_fn: *finalize,
                                    finalized_value: None,
                                })),
                                _ => unreachable!("scalar function called in aggregate context"),
                            },
                        };
                    }
                    match func {
                        AggFunc::Avg => {
                            let col = state.registers[*col].clone();
                            let Register::Aggregate(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Avg(acc, count) = agg.borrow_mut() else {
                                unreachable!();
                            };
                            *acc = exec_add(acc, col.get_owned_value());
                            *count += 1;
                        }
                        AggFunc::Sum | AggFunc::Total => {
                            let col = state.registers[*col].clone();
                            let Register::Aggregate(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Sum(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };
                            match col {
                                Register::OwnedValue(owned_value) => {
                                    *acc += owned_value;
                                }
                                _ => unreachable!(),
                            }
                        }
                        AggFunc::Count | AggFunc::Count0 => {
                            let col = state.registers[*col].get_owned_value().clone();
                            if matches!(
                                &state.registers[*acc_reg],
                                Register::OwnedValue(OwnedValue::Null)
                            ) {
                                state.registers[*acc_reg] =
                                    Register::Aggregate(AggContext::Count(OwnedValue::Integer(0)));
                            }
                            let Register::Aggregate(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Count(count) = agg.borrow_mut() else {
                                unreachable!();
                            };

                            if !(matches!(func, AggFunc::Count) && matches!(col, OwnedValue::Null))
                            {
                                *count += 1;
                            };
                        }
                        AggFunc::Max => {
                            let col = state.registers[*col].clone();
                            let Register::Aggregate(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Max(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };

                            match (acc.as_mut(), col.get_owned_value()) {
                                (None, value) => {
                                    *acc = Some(value.clone());
                                }
                                (
                                    Some(OwnedValue::Integer(ref mut current_max)),
                                    OwnedValue::Integer(value),
                                ) => {
                                    if *value > *current_max {
                                        *current_max = value.clone();
                                    }
                                }
                                (
                                    Some(OwnedValue::Float(ref mut current_max)),
                                    OwnedValue::Float(value),
                                ) => {
                                    if *value > *current_max {
                                        *current_max = *value;
                                    }
                                }
                                (
                                    Some(OwnedValue::Text(ref mut current_max)),
                                    OwnedValue::Text(value),
                                ) => {
                                    if value.value > current_max.value {
                                        *current_max = value.clone();
                                    }
                                }
                                _ => {
                                    eprintln!("Unexpected types in max aggregation");
                                }
                            }
                        }
                        AggFunc::Min => {
                            let col = state.registers[*col].clone();
                            let Register::Aggregate(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::Min(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };

                            match (acc.as_mut(), col.get_owned_value()) {
                                (None, value) => {
                                    *acc.borrow_mut() = Some(value.clone());
                                }
                                (
                                    Some(OwnedValue::Integer(ref mut current_min)),
                                    OwnedValue::Integer(value),
                                ) => {
                                    if *value < *current_min {
                                        *current_min = *value;
                                    }
                                }
                                (
                                    Some(OwnedValue::Float(ref mut current_min)),
                                    OwnedValue::Float(value),
                                ) => {
                                    if *value < *current_min {
                                        *current_min = *value;
                                    }
                                }
                                (
                                    Some(OwnedValue::Text(ref mut current_min)),
                                    OwnedValue::Text(text),
                                ) => {
                                    if text.value < current_min.value {
                                        *current_min = text.clone();
                                    }
                                }
                                _ => {
                                    eprintln!("Unexpected types in min aggregation");
                                }
                            }
                        }
                        AggFunc::GroupConcat | AggFunc::StringAgg => {
                            let col = state.registers[*col].get_owned_value().clone();
                            let delimiter = state.registers[*delimiter].clone();
                            let Register::Aggregate(agg) = state.registers[*acc_reg].borrow_mut()
                            else {
                                unreachable!();
                            };
                            let AggContext::GroupConcat(acc) = agg.borrow_mut() else {
                                unreachable!();
                            };
                            if acc.to_string().is_empty() {
                                *acc = col;
                            } else {
                                match delimiter {
                                    Register::OwnedValue(owned_value) => {
                                        *acc += owned_value;
                                    }
                                    _ => unreachable!(),
                                }
                                *acc += col;
                            }
                        }
                        AggFunc::External(_) => {
                            let (step_fn, state_ptr, argc) = {
                                let Register::Aggregate(agg) = &state.registers[*acc_reg] else {
                                    unreachable!();
                                };
                                let AggContext::External(agg_state) = agg else {
                                    unreachable!();
                                };
                                (agg_state.step_fn, agg_state.state, agg_state.argc)
                            };
                            if argc == 0 {
                                unsafe { step_fn(state_ptr, 0, std::ptr::null()) };
                            } else {
                                let register_slice = &state.registers[*col..*col + argc];
                                let mut ext_values: Vec<ExtValue> = Vec::with_capacity(argc);
                                for ov in register_slice.iter() {
                                    ext_values.push(ov.get_owned_value().to_ffi());
                                }
                                let argv_ptr = ext_values.as_ptr();
                                unsafe { step_fn(state_ptr, argc as i32, argv_ptr) };
                                for ext_value in ext_values {
                                    unsafe { ext_value.__free_internal_type() };
                                }
                            }
                        }
                    };
                    state.pc += 1;
                }
                Insn::AggFinal { register, func } => {
                    match state.registers[*register].borrow_mut() {
                        Register::Aggregate(agg) => match func {
                            AggFunc::Avg => {
                                let AggContext::Avg(acc, count) = agg.borrow_mut() else {
                                    unreachable!();
                                };
                                *acc /= count.clone();
                                state.registers[*register] = Register::OwnedValue(acc.clone());
                            }
                            AggFunc::Sum | AggFunc::Total => {
                                let AggContext::Sum(acc) = agg.borrow_mut() else {
                                    unreachable!();
                                };
                                let value = match acc {
                                    OwnedValue::Integer(i) => OwnedValue::Integer(*i),
                                    OwnedValue::Float(f) => OwnedValue::Float(*f),
                                    _ => OwnedValue::Float(0.0),
                                };
                                state.registers[*register] = Register::OwnedValue(value);
                            }
                            AggFunc::Count | AggFunc::Count0 => {
                                let AggContext::Count(count) = agg.borrow_mut() else {
                                    unreachable!();
                                };
                                state.registers[*register] = Register::OwnedValue(count.clone());
                            }
                            AggFunc::Max => {
                                let AggContext::Max(acc) = agg.borrow_mut() else {
                                    unreachable!();
                                };
                                match acc {
                                    Some(value) => {
                                        state.registers[*register] =
                                            Register::OwnedValue(value.clone())
                                    }
                                    None => {
                                        state.registers[*register] =
                                            Register::OwnedValue(OwnedValue::Null)
                                    }
                                }
                            }
                            AggFunc::Min => {
                                let AggContext::Min(acc) = agg.borrow_mut() else {
                                    unreachable!();
                                };
                                match acc {
                                    Some(value) => {
                                        state.registers[*register] =
                                            Register::OwnedValue(value.clone())
                                    }
                                    None => {
                                        state.registers[*register] =
                                            Register::OwnedValue(OwnedValue::Null)
                                    }
                                }
                            }
                            AggFunc::GroupConcat | AggFunc::StringAgg => {
                                let AggContext::GroupConcat(acc) = agg.borrow_mut() else {
                                    unreachable!();
                                };
                                state.registers[*register] = Register::OwnedValue(acc.clone());
                            }
                            AggFunc::External(_) => {
                                agg.compute_external()?;
                                let AggContext::External(agg_state) = agg.borrow_mut() else {
                                    unreachable!();
                                };
                                match &agg_state.finalized_value {
                                    Some(value) => {
                                        state.registers[*register] =
                                            Register::OwnedValue(value.clone())
                                    }
                                    None => {
                                        state.registers[*register] =
                                            Register::OwnedValue(OwnedValue::Null)
                                    }
                                }
                            }
                        },
                        Register::OwnedValue(OwnedValue::Null) => {
                            // when the set is empty
                            match func {
                                AggFunc::Total => {
                                    state.registers[*register] =
                                        Register::OwnedValue(OwnedValue::Float(0.0));
                                }
                                AggFunc::Count | AggFunc::Count0 => {
                                    state.registers[*register] =
                                        Register::OwnedValue(OwnedValue::Integer(0));
                                }
                                _ => {}
                            }
                        }
                        _ => {
                            unreachable!();
                        }
                    };
                    state.pc += 1;
                }
                Insn::SorterOpen {
                    cursor_id,
                    columns: _,
                    order,
                } => {
                    let order = order
                        .get_values()
                        .iter()
                        .map(|v| match v {
                            OwnedValue::Integer(i) => *i == 0,
                            _ => unreachable!(),
                        })
                        .collect();
                    let cursor = Sorter::new(order);
                    let mut cursors = state.cursors.borrow_mut();
                    cursors
                        .get_mut(*cursor_id)
                        .unwrap()
                        .replace(Cursor::new_sorter(cursor));
                    state.pc += 1;
                }
                Insn::SorterData {
                    cursor_id,
                    dest_reg,
                    pseudo_cursor,
                } => {
                    let record = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_sorter_mut();
                        cursor.record().map(|r| r.clone())
                    };
                    let record = match record {
                        Some(record) => record,
                        None => {
                            state.pc += 1;
                            continue;
                        }
                    };
                    state.registers[*dest_reg] = Register::Record(record.clone());
                    {
                        let mut pseudo_cursor = state.get_cursor(*pseudo_cursor);
                        pseudo_cursor.as_pseudo_mut().insert(record);
                    }
                    state.pc += 1;
                }
                Insn::SorterInsert {
                    cursor_id,
                    record_reg,
                } => {
                    {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_sorter_mut();
                        let record = match &state.registers[*record_reg] {
                            Register::Record(record) => record,
                            _ => unreachable!("SorterInsert on non-record register"),
                        };
                        cursor.insert(record);
                    }
                    state.pc += 1;
                }
                Insn::SorterSort {
                    cursor_id,
                    pc_if_empty,
                } => {
                    let is_empty = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_sorter_mut();
                        let is_empty = cursor.is_empty();
                        if !is_empty {
                            cursor.sort();
                        }
                        is_empty
                    };
                    if is_empty {
                        state.pc = pc_if_empty.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::SorterNext {
                    cursor_id,
                    pc_if_next,
                } => {
                    assert!(pc_if_next.is_offset());
                    let has_more = {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_sorter_mut();
                        cursor.next();
                        cursor.has_more()
                    };
                    if has_more {
                        state.pc = pc_if_next.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::Function {
                    constant_mask,
                    func,
                    start_reg,
                    dest,
                } => {
                    let arg_count = func.arg_count;

                    match &func.func {
                        #[cfg(feature = "json")]
                        crate::function::Func::Json(json_func) => match json_func {
                            JsonFunc::Json => {
                                let json_value = &state.registers[*start_reg];
                                let json_str = get_json(json_value.get_owned_value(), None);
                                match json_str {
                                    Ok(json) => state.registers[*dest] = Register::OwnedValue(json),
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::Jsonb => {
                                let json_value = &state.registers[*start_reg];
                                let json_blob =
                                    jsonb(json_value.get_owned_value(), &state.json_cache);
                                match json_blob {
                                    Ok(json) => state.registers[*dest] = Register::OwnedValue(json),
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::JsonArray
                            | JsonFunc::JsonObject
                            | JsonFunc::JsonbArray
                            | JsonFunc::JsonbObject => {
                                let reg_values =
                                    &state.registers[*start_reg..*start_reg + arg_count];

                                let json_func = match json_func {
                                    JsonFunc::JsonArray => json_array,
                                    JsonFunc::JsonObject => json_object,
                                    JsonFunc::JsonbArray => jsonb_array,
                                    JsonFunc::JsonbObject => jsonb_object,
                                    _ => unreachable!(),
                                };
                                let json_result = json_func(reg_values);

                                match json_result {
                                    Ok(json) => state.registers[*dest] = Register::OwnedValue(json),
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::JsonExtract => {
                                let result = match arg_count {
                                    0 => Ok(OwnedValue::Null),
                                    _ => {
                                        let val = &state.registers[*start_reg];
                                        let reg_values = &state.registers
                                            [*start_reg + 1..*start_reg + arg_count];

                                        json_extract(
                                            val.get_owned_value(),
                                            reg_values,
                                            &state.json_cache,
                                        )
                                    }
                                };

                                match result {
                                    Ok(json) => state.registers[*dest] = Register::OwnedValue(json),
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::JsonbExtract => {
                                let result = match arg_count {
                                    0 => Ok(OwnedValue::Null),
                                    _ => {
                                        let val = &state.registers[*start_reg];
                                        let reg_values = &state.registers
                                            [*start_reg + 1..*start_reg + arg_count];

                                        jsonb_extract(
                                            val.get_owned_value(),
                                            reg_values,
                                            &state.json_cache,
                                        )
                                    }
                                };

                                match result {
                                    Ok(json) => state.registers[*dest] = Register::OwnedValue(json),
                                    Err(e) => return Err(e),
                                }
                            }

                            JsonFunc::JsonArrowExtract | JsonFunc::JsonArrowShiftExtract => {
                                assert_eq!(arg_count, 2);
                                let json = &state.registers[*start_reg];
                                let path = &state.registers[*start_reg + 1];
                                let json_func = match json_func {
                                    JsonFunc::JsonArrowExtract => json_arrow_extract,
                                    JsonFunc::JsonArrowShiftExtract => json_arrow_shift_extract,
                                    _ => unreachable!(),
                                };
                                let json_str = json_func(
                                    json.get_owned_value(),
                                    path.get_owned_value(),
                                    &state.json_cache,
                                );
                                match json_str {
                                    Ok(json) => state.registers[*dest] = Register::OwnedValue(json),
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::JsonArrayLength | JsonFunc::JsonType => {
                                let json_value = &state.registers[*start_reg];
                                let path_value = if arg_count > 1 {
                                    Some(&state.registers[*start_reg + 1])
                                } else {
                                    None
                                };
                                let func_result = match json_func {
                                    JsonFunc::JsonArrayLength => json_array_length(
                                        json_value.get_owned_value(),
                                        path_value.map(|x| x.get_owned_value()),
                                        &state.json_cache,
                                    ),
                                    JsonFunc::JsonType => json_type(
                                        json_value.get_owned_value(),
                                        path_value.map(|x| x.get_owned_value()),
                                    ),
                                    _ => unreachable!(),
                                };

                                match func_result {
                                    Ok(result) => {
                                        state.registers[*dest] = Register::OwnedValue(result)
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::JsonErrorPosition => {
                                let json_value = &state.registers[*start_reg];
                                match json_error_position(json_value.get_owned_value()) {
                                    Ok(pos) => state.registers[*dest] = Register::OwnedValue(pos),
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::JsonValid => {
                                let json_value = &state.registers[*start_reg];
                                state.registers[*dest] = Register::OwnedValue(is_json_valid(
                                    json_value.get_owned_value(),
                                ));
                            }
                            JsonFunc::JsonPatch => {
                                assert_eq!(arg_count, 2);
                                assert!(*start_reg + 1 < state.registers.len());
                                let target = &state.registers[*start_reg];
                                let patch = &state.registers[*start_reg + 1];
                                state.registers[*dest] = Register::OwnedValue(json_patch(
                                    target.get_owned_value(),
                                    patch.get_owned_value(),
                                )?);
                            }
                            JsonFunc::JsonRemove => {
                                if let Ok(json) = json_remove(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                    &state.json_cache,
                                ) {
                                    state.registers[*dest] = Register::OwnedValue(json);
                                } else {
                                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                                }
                            }
                            JsonFunc::JsonbRemove => {
                                if let Ok(json) = jsonb_remove(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                    &state.json_cache,
                                ) {
                                    state.registers[*dest] = Register::OwnedValue(json);
                                } else {
                                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                                }
                            }
                            JsonFunc::JsonReplace => {
                                if let Ok(json) = json_replace(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                    &state.json_cache,
                                ) {
                                    state.registers[*dest] = Register::OwnedValue(json);
                                } else {
                                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                                }
                            }
                            JsonFunc::JsonbReplace => {
                                if let Ok(json) = jsonb_replace(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                    &state.json_cache,
                                ) {
                                    state.registers[*dest] = Register::OwnedValue(json);
                                } else {
                                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                                }
                            }
                            JsonFunc::JsonInsert => {
                                if let Ok(json) = json_insert(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                    &state.json_cache,
                                ) {
                                    state.registers[*dest] = Register::OwnedValue(json);
                                } else {
                                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                                }
                            }
                            JsonFunc::JsonbInsert => {
                                if let Ok(json) = jsonb_insert(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                    &state.json_cache,
                                ) {
                                    state.registers[*dest] = Register::OwnedValue(json);
                                } else {
                                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                                }
                            }
                            JsonFunc::JsonPretty => {
                                let json_value = &state.registers[*start_reg];
                                let indent = if arg_count > 1 {
                                    Some(&state.registers[*start_reg + 1])
                                } else {
                                    None
                                };

                                // Blob should be converted to Ascii in a lossy way
                                // However, Rust strings uses utf-8
                                // so the behavior at the moment is slightly different
                                // To the way blobs are parsed here in SQLite.
                                let indent = match indent {
                                    Some(value) => match value.get_owned_value() {
                                        OwnedValue::Text(text) => text.as_str(),
                                        OwnedValue::Integer(val) => &val.to_string(),
                                        OwnedValue::Float(val) => &val.to_string(),
                                        OwnedValue::Blob(val) => &String::from_utf8_lossy(val),
                                        _ => "    ",
                                    },
                                    // If the second argument is omitted or is NULL, then indentation is four spaces per level
                                    None => "    ",
                                };

                                let json_str =
                                    get_json(json_value.get_owned_value(), Some(indent))?;
                                state.registers[*dest] = Register::OwnedValue(json_str);
                            }
                            JsonFunc::JsonSet => {
                                if arg_count % 2 == 0 {
                                    bail_constraint_error!(
                                        "json_set() needs an odd number of arguments"
                                    )
                                }
                                let reg_values =
                                    &state.registers[*start_reg..*start_reg + arg_count];

                                let json_result = json_set(reg_values, &state.json_cache);

                                match json_result {
                                    Ok(json) => state.registers[*dest] = Register::OwnedValue(json),
                                    Err(e) => return Err(e),
                                }
                            }
                            JsonFunc::JsonQuote => {
                                let json_value = &state.registers[*start_reg];

                                match json_quote(json_value.get_owned_value()) {
                                    Ok(result) => {
                                        state.registers[*dest] = Register::OwnedValue(result)
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                        },
                        crate::function::Func::Scalar(scalar_func) => match scalar_func {
                            ScalarFunc::Cast => {
                                assert_eq!(arg_count, 2);
                                assert!(*start_reg + 1 < state.registers.len());
                                let reg_value_argument = state.registers[*start_reg].clone();
                                let OwnedValue::Text(reg_value_type) =
                                    state.registers[*start_reg + 1].get_owned_value().clone()
                                else {
                                    unreachable!("Cast with non-text type");
                                };
                                let result = exec_cast(
                                    &reg_value_argument.get_owned_value(),
                                    reg_value_type.as_str(),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Changes => {
                                let res = &self.connection.upgrade().unwrap().last_change;
                                let changes = res.get();
                                state.registers[*dest] =
                                    Register::OwnedValue(OwnedValue::Integer(changes));
                            }
                            ScalarFunc::Char => {
                                let reg_values =
                                    &state.registers[*start_reg..*start_reg + arg_count];
                                state.registers[*dest] =
                                    Register::OwnedValue(exec_char(reg_values));
                            }
                            ScalarFunc::Coalesce => {}
                            ScalarFunc::Concat => {
                                let reg_values =
                                    &state.registers[*start_reg..*start_reg + arg_count];
                                let result = exec_concat_strings(reg_values);
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::ConcatWs => {
                                let reg_values =
                                    &state.registers[*start_reg..*start_reg + arg_count];
                                let result = exec_concat_ws(reg_values);
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Glob => {
                                let pattern = &state.registers[*start_reg];
                                let text = &state.registers[*start_reg + 1];
                                let result =
                                    match (pattern.get_owned_value(), text.get_owned_value()) {
                                        (OwnedValue::Text(pattern), OwnedValue::Text(text)) => {
                                            let cache = if *constant_mask > 0 {
                                                Some(&mut state.regex_cache.glob)
                                            } else {
                                                None
                                            };
                                            OwnedValue::Integer(exec_glob(
                                                cache,
                                                pattern.as_str(),
                                                text.as_str(),
                                            )
                                                as i64)
                                        }
                                        _ => {
                                            unreachable!("Like on non-text registers");
                                        }
                                    };
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::IfNull => {}
                            ScalarFunc::Iif => {}
                            ScalarFunc::Instr => {
                                let reg_value = &state.registers[*start_reg];
                                let pattern_value = &state.registers[*start_reg + 1];
                                let result = exec_instr(
                                    reg_value.get_owned_value(),
                                    pattern_value.get_owned_value(),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::LastInsertRowid => {
                                if let Some(conn) = self.connection.upgrade() {
                                    state.registers[*dest] = Register::OwnedValue(
                                        OwnedValue::Integer(conn.last_insert_rowid() as i64),
                                    );
                                } else {
                                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Null);
                                }
                            }
                            ScalarFunc::Like => {
                                let pattern = &state.registers[*start_reg];
                                let match_expression = &state.registers[*start_reg + 1];

                                let pattern = match pattern.get_owned_value() {
                                    OwnedValue::Text(_) => pattern.get_owned_value(),
                                    _ => &exec_cast(pattern.get_owned_value(), "TEXT"),
                                };
                                let match_expression = match match_expression.get_owned_value() {
                                    OwnedValue::Text(_) => match_expression.get_owned_value(),
                                    _ => &exec_cast(match_expression.get_owned_value(), "TEXT"),
                                };

                                let result = match (pattern, match_expression) {
                                    (
                                        OwnedValue::Text(pattern),
                                        OwnedValue::Text(match_expression),
                                    ) if arg_count == 3 => {
                                        let escape = match construct_like_escape_arg(
                                            state.registers[*start_reg + 2].get_owned_value(),
                                        ) {
                                            Ok(x) => x,
                                            Err(e) => return Err(e),
                                        };

                                        OwnedValue::Integer(exec_like_with_escape(
                                            pattern.as_str(),
                                            match_expression.as_str(),
                                            escape,
                                        )
                                            as i64)
                                    }
                                    (
                                        OwnedValue::Text(pattern),
                                        OwnedValue::Text(match_expression),
                                    ) => {
                                        let cache = if *constant_mask > 0 {
                                            Some(&mut state.regex_cache.like)
                                        } else {
                                            None
                                        };
                                        OwnedValue::Integer(exec_like(
                                            cache,
                                            pattern.as_str(),
                                            match_expression.as_str(),
                                        )
                                            as i64)
                                    }
                                    (OwnedValue::Null, _) | (_, OwnedValue::Null) => {
                                        OwnedValue::Null
                                    }
                                    _ => {
                                        unreachable!("Like failed");
                                    }
                                };

                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Abs
                            | ScalarFunc::Lower
                            | ScalarFunc::Upper
                            | ScalarFunc::Length
                            | ScalarFunc::OctetLength
                            | ScalarFunc::Typeof
                            | ScalarFunc::Unicode
                            | ScalarFunc::Quote
                            | ScalarFunc::RandomBlob
                            | ScalarFunc::Sign
                            | ScalarFunc::Soundex
                            | ScalarFunc::ZeroBlob => {
                                let reg_value =
                                    state.registers[*start_reg].borrow_mut().get_owned_value();
                                let result = match scalar_func {
                                    ScalarFunc::Sign => exec_sign(reg_value),
                                    ScalarFunc::Abs => Some(exec_abs(reg_value)?),
                                    ScalarFunc::Lower => exec_lower(reg_value),
                                    ScalarFunc::Upper => exec_upper(reg_value),
                                    ScalarFunc::Length => Some(exec_length(reg_value)),
                                    ScalarFunc::OctetLength => Some(exec_octet_length(reg_value)),
                                    ScalarFunc::Typeof => Some(exec_typeof(reg_value)),
                                    ScalarFunc::Unicode => Some(exec_unicode(reg_value)),
                                    ScalarFunc::Quote => Some(exec_quote(reg_value)),
                                    ScalarFunc::RandomBlob => Some(exec_randomblob(reg_value)),
                                    ScalarFunc::ZeroBlob => Some(exec_zeroblob(reg_value)),
                                    ScalarFunc::Soundex => Some(exec_soundex(reg_value)),
                                    _ => unreachable!(),
                                };
                                state.registers[*dest] =
                                    Register::OwnedValue(result.unwrap_or(OwnedValue::Null));
                            }
                            ScalarFunc::Hex => {
                                let reg_value = state.registers[*start_reg].borrow_mut();
                                let result = exec_hex(reg_value.get_owned_value());
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Unhex => {
                                let reg_value = &state.registers[*start_reg];
                                let ignored_chars = state.registers.get(*start_reg + 1);
                                let result = exec_unhex(
                                    reg_value.get_owned_value(),
                                    ignored_chars.map(|x| x.get_owned_value()),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Random => {
                                state.registers[*dest] = Register::OwnedValue(exec_random());
                            }
                            ScalarFunc::Trim => {
                                let reg_value = &state.registers[*start_reg];
                                let pattern_value = if func.arg_count == 2 {
                                    state.registers.get(*start_reg + 1)
                                } else {
                                    None
                                };
                                let result = exec_trim(
                                    reg_value.get_owned_value(),
                                    pattern_value.map(|x| x.get_owned_value()),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::LTrim => {
                                let reg_value = &state.registers[*start_reg];
                                let pattern_value = if func.arg_count == 2 {
                                    state.registers.get(*start_reg + 1)
                                } else {
                                    None
                                };
                                let result = exec_ltrim(
                                    reg_value.get_owned_value(),
                                    pattern_value.map(|x| x.get_owned_value()),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::RTrim => {
                                let reg_value = &state.registers[*start_reg];
                                let pattern_value = if func.arg_count == 2 {
                                    state.registers.get(*start_reg + 1)
                                } else {
                                    None
                                };
                                let result = exec_rtrim(
                                    &reg_value.get_owned_value(),
                                    pattern_value.map(|x| x.get_owned_value()),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Round => {
                                let reg_value = &state.registers[*start_reg];
                                assert!(arg_count == 1 || arg_count == 2);
                                let precision_value = if arg_count > 1 {
                                    state.registers.get(*start_reg + 1)
                                } else {
                                    None
                                };
                                let result = exec_round(
                                    reg_value.get_owned_value(),
                                    precision_value.map(|x| x.get_owned_value()),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Min => {
                                let reg_values =
                                    &state.registers[*start_reg..*start_reg + arg_count];
                                state.registers[*dest] = Register::OwnedValue(exec_min(reg_values));
                            }
                            ScalarFunc::Max => {
                                let reg_values =
                                    &state.registers[*start_reg..*start_reg + arg_count];
                                state.registers[*dest] = Register::OwnedValue(exec_max(reg_values));
                            }
                            ScalarFunc::Nullif => {
                                let first_value = &state.registers[*start_reg];
                                let second_value = &state.registers[*start_reg + 1];
                                state.registers[*dest] = Register::OwnedValue(exec_nullif(
                                    first_value.get_owned_value(),
                                    second_value.get_owned_value(),
                                ));
                            }
                            ScalarFunc::Substr | ScalarFunc::Substring => {
                                let str_value = &state.registers[*start_reg];
                                let start_value = &state.registers[*start_reg + 1];
                                let length_value = if func.arg_count == 3 {
                                    Some(&state.registers[*start_reg + 2])
                                } else {
                                    None
                                };
                                let result = exec_substring(
                                    str_value.get_owned_value(),
                                    start_value.get_owned_value(),
                                    length_value.map(|x| x.get_owned_value()),
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Date => {
                                let result =
                                    exec_date(&state.registers[*start_reg..*start_reg + arg_count]);
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Time => {
                                let values = &state.registers[*start_reg..*start_reg + arg_count];
                                let result = exec_time(values);
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::TotalChanges => {
                                let res = &self.connection.upgrade().unwrap().total_changes;
                                let total_changes = res.get();
                                state.registers[*dest] =
                                    Register::OwnedValue(OwnedValue::Integer(total_changes));
                            }
                            ScalarFunc::DateTime => {
                                let result = exec_datetime_full(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::JulianDay => {
                                if *start_reg == 0 {
                                    let julianday: String =
                                        exec_julianday(&OwnedValue::build_text("now"))?;
                                    state.registers[*dest] =
                                        Register::OwnedValue(OwnedValue::build_text(&julianday));
                                } else {
                                    let datetime_value = &state.registers[*start_reg];
                                    let julianday =
                                        exec_julianday(datetime_value.get_owned_value());
                                    match julianday {
                                        Ok(time) => {
                                            state.registers[*dest] =
                                                Register::OwnedValue(OwnedValue::build_text(&time))
                                        }
                                        Err(e) => {
                                            return Err(LimboError::ParseError(format!(
                                                "Error encountered while parsing datetime value: {}",
                                                e
                                            )));
                                        }
                                    }
                                }
                            }
                            ScalarFunc::UnixEpoch => {
                                if *start_reg == 0 {
                                    let unixepoch: String =
                                        exec_unixepoch(&OwnedValue::build_text("now"))?;
                                    state.registers[*dest] =
                                        Register::OwnedValue(OwnedValue::build_text(&unixepoch));
                                } else {
                                    let datetime_value = &state.registers[*start_reg];
                                    let unixepoch =
                                        exec_unixepoch(datetime_value.get_owned_value());
                                    match unixepoch {
                                        Ok(time) => {
                                            state.registers[*dest] =
                                                Register::OwnedValue(OwnedValue::build_text(&time))
                                        }
                                        Err(e) => {
                                            return Err(LimboError::ParseError(format!(
                                                "Error encountered while parsing datetime value: {}",
                                                e
                                            )));
                                        }
                                    }
                                }
                            }
                            ScalarFunc::SqliteVersion => {
                                let version_integer: i64 =
                                    DATABASE_VERSION.get().unwrap().parse()?;
                                let version = execute_sqlite_version(version_integer);
                                state.registers[*dest] =
                                    Register::OwnedValue(OwnedValue::build_text(&version));
                            }
                            ScalarFunc::SqliteSourceId => {
                                let src_id = format!(
                                    "{} {}",
                                    info::build::BUILT_TIME_SQLITE,
                                    info::build::GIT_COMMIT_HASH.unwrap_or("unknown")
                                );
                                state.registers[*dest] =
                                    Register::OwnedValue(OwnedValue::build_text(&src_id));
                            }
                            ScalarFunc::Replace => {
                                assert_eq!(arg_count, 3);
                                let source = &state.registers[*start_reg];
                                let pattern = &state.registers[*start_reg + 1];
                                let replacement = &state.registers[*start_reg + 2];
                                state.registers[*dest] = Register::OwnedValue(exec_replace(
                                    source.get_owned_value(),
                                    pattern.get_owned_value(),
                                    replacement.get_owned_value(),
                                ));
                            }
                            #[cfg(feature = "fs")]
                            ScalarFunc::LoadExtension => {
                                let extension = &state.registers[*start_reg];
                                let ext =
                                    resolve_ext_path(&extension.get_owned_value().to_string())?;
                                if let Some(conn) = self.connection.upgrade() {
                                    conn.load_extension(ext)?;
                                }
                            }
                            ScalarFunc::StrfTime => {
                                let result = exec_strftime(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            ScalarFunc::Printf => {
                                let result = exec_printf(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                )?;
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                        },
                        crate::function::Func::Vector(vector_func) => match vector_func {
                            VectorFunc::Vector => {
                                let result =
                                    vector32(&state.registers[*start_reg..*start_reg + arg_count])?;
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            VectorFunc::Vector32 => {
                                let result =
                                    vector32(&state.registers[*start_reg..*start_reg + arg_count])?;
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            VectorFunc::Vector64 => {
                                let result =
                                    vector64(&state.registers[*start_reg..*start_reg + arg_count])?;
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            VectorFunc::VectorExtract => {
                                let result = vector_extract(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                )?;
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                            VectorFunc::VectorDistanceCos => {
                                let result = vector_distance_cos(
                                    &state.registers[*start_reg..*start_reg + arg_count],
                                )?;
                                state.registers[*dest] = Register::OwnedValue(result);
                            }
                        },
                        crate::function::Func::External(f) => match f.func {
                            ExtFunc::Scalar(f) => {
                                if arg_count == 0 {
                                    let result_c_value: ExtValue =
                                        unsafe { (f)(0, std::ptr::null()) };
                                    match OwnedValue::from_ffi(result_c_value) {
                                        Ok(result_ov) => {
                                            state.registers[*dest] =
                                                Register::OwnedValue(result_ov);
                                        }
                                        Err(e) => {
                                            return Err(e);
                                        }
                                    }
                                } else {
                                    let register_slice =
                                        &state.registers[*start_reg..*start_reg + arg_count];
                                    let mut ext_values: Vec<ExtValue> =
                                        Vec::with_capacity(arg_count);
                                    for ov in register_slice.iter() {
                                        let val = ov.get_owned_value().to_ffi();
                                        ext_values.push(val);
                                    }
                                    let argv_ptr = ext_values.as_ptr();
                                    let result_c_value: ExtValue =
                                        unsafe { (f)(arg_count as i32, argv_ptr) };
                                    match OwnedValue::from_ffi(result_c_value) {
                                        Ok(result_ov) => {
                                            state.registers[*dest] =
                                                Register::OwnedValue(result_ov);
                                        }
                                        Err(e) => {
                                            return Err(e);
                                        }
                                    }
                                }
                            }
                            _ => unreachable!("aggregate called in scalar context"),
                        },
                        crate::function::Func::Math(math_func) => match math_func.arity() {
                            MathFuncArity::Nullary => match math_func {
                                MathFunc::Pi => {
                                    state.registers[*dest] = Register::OwnedValue(
                                        OwnedValue::Float(std::f64::consts::PI),
                                    );
                                }
                                _ => {
                                    unreachable!(
                                        "Unexpected mathematical Nullary function {:?}",
                                        math_func
                                    );
                                }
                            },

                            MathFuncArity::Unary => {
                                let reg_value = &state.registers[*start_reg];
                                let result =
                                    exec_math_unary(reg_value.get_owned_value(), math_func);
                                state.registers[*dest] = Register::OwnedValue(result);
                            }

                            MathFuncArity::Binary => {
                                let lhs = &state.registers[*start_reg];
                                let rhs = &state.registers[*start_reg + 1];
                                let result = exec_math_binary(
                                    lhs.get_owned_value(),
                                    rhs.get_owned_value(),
                                    math_func,
                                );
                                state.registers[*dest] = Register::OwnedValue(result);
                            }

                            MathFuncArity::UnaryOrBinary => match math_func {
                                MathFunc::Log => {
                                    let result = match arg_count {
                                        1 => {
                                            let arg = &state.registers[*start_reg];
                                            exec_math_log(arg.get_owned_value(), None)
                                        }
                                        2 => {
                                            let base = &state.registers[*start_reg];
                                            let arg = &state.registers[*start_reg + 1];
                                            exec_math_log(
                                                arg.get_owned_value(),
                                                Some(base.get_owned_value()),
                                            )
                                        }
                                        _ => unreachable!(
                                            "{:?} function with unexpected number of arguments",
                                            math_func
                                        ),
                                    };
                                    state.registers[*dest] = Register::OwnedValue(result);
                                }
                                _ => unreachable!(
                                    "Unexpected mathematical UnaryOrBinary function {:?}",
                                    math_func
                                ),
                            },
                        },
                        crate::function::Func::Agg(_) => {
                            unreachable!("Aggregate functions should not be handled here")
                        }
                    }
                    state.pc += 1;
                }
                Insn::InitCoroutine {
                    yield_reg,
                    jump_on_definition,
                    start_offset,
                } => {
                    assert!(jump_on_definition.is_offset());
                    let start_offset = start_offset.to_offset_int();
                    state.registers[*yield_reg] =
                        Register::OwnedValue(OwnedValue::Integer(start_offset as i64));
                    state.ended_coroutine.unset(*yield_reg);
                    let jump_on_definition = jump_on_definition.to_offset_int();
                    state.pc = if jump_on_definition == 0 {
                        state.pc + 1
                    } else {
                        jump_on_definition
                    };
                }
                Insn::EndCoroutine { yield_reg } => {
                    if let OwnedValue::Integer(pc) = state.registers[*yield_reg].get_owned_value() {
                        state.ended_coroutine.set(*yield_reg);
                        let pc: u32 = (*pc)
                            .try_into()
                            .unwrap_or_else(|_| panic!("EndCoroutine: pc overflow: {}", pc));
                        state.pc = pc - 1; // yield jump is always next to yield. Here we subtract 1 to go back to yield instruction
                    } else {
                        unreachable!();
                    }
                }
                Insn::Yield {
                    yield_reg,
                    end_offset,
                } => {
                    if let OwnedValue::Integer(pc) = state.registers[*yield_reg].get_owned_value() {
                        if state.ended_coroutine.get(*yield_reg) {
                            state.pc = end_offset.to_offset_int();
                        } else {
                            let pc: u32 = (*pc)
                                .try_into()
                                .unwrap_or_else(|_| panic!("Yield: pc overflow: {}", pc));
                            // swap the program counter with the value in the yield register
                            // this is the mechanism that allows jumping back and forth between the coroutine and the caller
                            (state.pc, state.registers[*yield_reg]) = (
                                pc,
                                Register::OwnedValue(OwnedValue::Integer((state.pc + 1) as i64)),
                            );
                        }
                    } else {
                        unreachable!(
                            "yield_reg {} contains non-integer value: {:?}",
                            *yield_reg, state.registers[*yield_reg]
                        );
                    }
                }
                Insn::InsertAsync {
                    cursor,
                    key_reg,
                    record_reg,
                    flag: _,
                } => {
                    {
                        let mut cursor = state.get_cursor(*cursor);
                        let cursor = cursor.as_btree_mut();
                        let record = match &state.registers[*record_reg] {
                            Register::Record(r) => r,
                            _ => unreachable!("Not a record! Cannot insert a non record value."),
                        };
                        let key = &state.registers[*key_reg];
                        // NOTE(pere): Sending moved_before == true is okay because we moved before but
                        // if we were to set to false after starting a balance procedure, it might
                        // leave undefined state.
                        return_if_io!(cursor.insert(key.get_owned_value(), record, true));
                    }
                    state.pc += 1;
                }
                Insn::InsertAwait { cursor_id } => {
                    {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        cursor.wait_for_completion()?;
                        // Only update last_insert_rowid for regular table inserts, not schema modifications
                        if cursor.root_page() != 1 {
                            if let Some(rowid) = cursor.rowid()? {
                                if let Some(conn) = self.connection.upgrade() {
                                    conn.update_last_rowid(rowid);
                                }
                                let prev_changes = self.n_change.get();
                                self.n_change.set(prev_changes + 1);
                            }
                        }
                    }
                    state.pc += 1;
                }
                Insn::DeleteAsync { cursor_id } => {
                    {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        return_if_io!(cursor.delete());
                    }
                    state.pc += 1;
                }
                Insn::DeleteAwait { cursor_id } => {
                    {
                        let mut cursor = state.get_cursor(*cursor_id);
                        let cursor = cursor.as_btree_mut();
                        cursor.wait_for_completion()?;
                    }
                    let prev_changes = self.n_change.get();
                    self.n_change.set(prev_changes + 1);
                    state.pc += 1;
                }
                Insn::NewRowid {
                    cursor, rowid_reg, ..
                } => {
                    let rowid = {
                        let mut cursor = state.get_cursor(*cursor);
                        let cursor = cursor.as_btree_mut();
                        // TODO: make io handle rng
                        let rowid = return_if_io!(get_new_rowid(cursor, thread_rng()));
                        rowid
                    };
                    state.registers[*rowid_reg] = Register::OwnedValue(OwnedValue::Integer(rowid));
                    state.pc += 1;
                }
                Insn::MustBeInt { reg } => {
                    match &state.registers[*reg].get_owned_value() {
                        OwnedValue::Integer(_) => {}
                        OwnedValue::Float(f) => match cast_real_to_integer(*f) {
                            Ok(i) => {
                                state.registers[*reg] = Register::OwnedValue(OwnedValue::Integer(i))
                            }
                            Err(_) => crate::bail_parse_error!(
                                "MustBeInt: the value in register cannot be cast to integer"
                            ),
                        },
                        OwnedValue::Text(text) => {
                            match checked_cast_text_to_numeric(text.as_str()) {
                                Ok(OwnedValue::Integer(i)) => {
                                    state.registers[*reg] =
                                        Register::OwnedValue(OwnedValue::Integer(i))
                                }
                                Ok(OwnedValue::Float(f)) => {
                                    state.registers[*reg] =
                                        Register::OwnedValue(OwnedValue::Integer(f as i64))
                                }
                                _ => crate::bail_parse_error!(
                                    "MustBeInt: the value in register cannot be cast to integer"
                                ),
                            }
                        }
                        _ => {
                            crate::bail_parse_error!(
                                "MustBeInt: the value in register cannot be cast to integer"
                            );
                        }
                    };
                    state.pc += 1;
                }
                Insn::SoftNull { reg } => {
                    state.registers[*reg] = Register::OwnedValue(OwnedValue::Null);
                    state.pc += 1;
                }
                Insn::NotExists {
                    cursor,
                    rowid_reg,
                    target_pc,
                } => {
                    let exists = {
                        let mut cursor =
                            must_be_btree_cursor!(*cursor, self.cursor_ref, state, "NotExists");
                        let cursor = cursor.as_btree_mut();
                        let exists = return_if_io!(
                            cursor.exists(state.registers[*rowid_reg].get_owned_value())
                        );
                        exists
                    };
                    if exists {
                        state.pc += 1;
                    } else {
                        state.pc = target_pc.to_offset_int();
                    }
                }
                Insn::OffsetLimit {
                    limit_reg,
                    combined_reg,
                    offset_reg,
                } => {
                    let limit_val = match state.registers[*limit_reg].get_owned_value() {
                        OwnedValue::Integer(val) => val,
                        _ => {
                            return Err(LimboError::InternalError(
                                "OffsetLimit: the value in limit_reg is not an integer".into(),
                            ));
                        }
                    };
                    let offset_val = match state.registers[*offset_reg].get_owned_value() {
                        OwnedValue::Integer(val) if *val < 0 => 0,
                        OwnedValue::Integer(val) if *val >= 0 => *val,
                        _ => {
                            return Err(LimboError::InternalError(
                                "OffsetLimit: the value in offset_reg is not an integer".into(),
                            ));
                        }
                    };

                    let offset_limit_sum = limit_val.overflowing_add(offset_val);
                    if *limit_val <= 0 || offset_limit_sum.1 {
                        state.registers[*combined_reg] =
                            Register::OwnedValue(OwnedValue::Integer(-1));
                    } else {
                        state.registers[*combined_reg] =
                            Register::OwnedValue(OwnedValue::Integer(offset_limit_sum.0));
                    }
                    state.pc += 1;
                }
                // this cursor may be reused for next insert
                // Update: tablemoveto is used to travers on not exists, on insert depending on flags if nonseek it traverses again.
                // If not there might be some optimizations obviously.
                Insn::OpenWriteAsync {
                    cursor_id,
                    root_page,
                } => {
                    let (_, cursor_type) = self.cursor_ref.get(*cursor_id).unwrap();
                    let mut cursors = state.cursors.borrow_mut();
                    let is_index = cursor_type.is_index();
                    let mv_cursor = match state.mv_tx_id {
                        Some(tx_id) => {
                            let table_id = *root_page as u64;
                            let mv_store = mv_store.as_ref().unwrap().clone();
                            let mv_cursor = Rc::new(RefCell::new(
                                MvCursor::new(mv_store, tx_id, table_id).unwrap(),
                            ));
                            Some(mv_cursor)
                        }
                        None => None,
                    };
                    let cursor = BTreeCursor::new(mv_cursor, pager.clone(), *root_page);
                    if is_index {
                        cursors
                            .get_mut(*cursor_id)
                            .unwrap()
                            .replace(Cursor::new_btree(cursor));
                    } else {
                        cursors
                            .get_mut(*cursor_id)
                            .unwrap()
                            .replace(Cursor::new_btree(cursor));
                    }
                    state.pc += 1;
                }
                Insn::OpenWriteAwait {} => {
                    state.pc += 1;
                }
                Insn::Copy {
                    src_reg,
                    dst_reg,
                    amount,
                } => {
                    for i in 0..=*amount {
                        state.registers[*dst_reg + i] = state.registers[*src_reg + i].clone();
                    }
                    state.pc += 1;
                }
                Insn::CreateBtree { db, root, flags } => {
                    if *db > 0 {
                        // TODO: implement temp databases
                        todo!("temp databases not implemented yet");
                    }
                    let root_page = pager.btree_create(*flags);
                    state.registers[*root] =
                        Register::OwnedValue(OwnedValue::Integer(root_page as i64));
                    state.pc += 1;
                }
                Insn::Destroy {
                    root,
                    former_root_reg: _,
                    is_temp,
                } => {
                    if *is_temp == 1 {
                        todo!("temp databases not implemented yet.");
                    }
                    let mut cursor = BTreeCursor::new(None, pager.clone(), *root);
                    cursor.btree_destroy()?;
                    state.pc += 1;
                }
                Insn::DropTable {
                    db,
                    _p2,
                    _p3,
                    table_name,
                } => {
                    if *db > 0 {
                        todo!("temp databases not implemented yet");
                    }
                    if let Some(conn) = self.connection.upgrade() {
                        let mut schema = conn.schema.write();
                        schema.remove_indices_for_table(table_name);
                        schema.remove_table(table_name);
                    }
                    state.pc += 1;
                }
                Insn::Close { cursor_id } => {
                    let mut cursors = state.cursors.borrow_mut();
                    cursors.get_mut(*cursor_id).unwrap().take();
                    state.pc += 1;
                }
                Insn::IsNull { reg, target_pc } => {
                    if matches!(
                        state.registers[*reg],
                        Register::OwnedValue(OwnedValue::Null)
                    ) {
                        state.pc = target_pc.to_offset_int();
                    } else {
                        state.pc += 1;
                    }
                }
                Insn::PageCount { db, dest } => {
                    if *db > 0 {
                        // TODO: implement temp databases
                        todo!("temp databases not implemented yet");
                    }
                    // SQLite returns "0" on an empty database, and 2 on the first insertion,
                    // so we'll mimic that behavior.
                    let mut pages = pager.db_header.lock().database_size.into();
                    if pages == 1 {
                        pages = 0;
                    }
                    state.registers[*dest] = Register::OwnedValue(OwnedValue::Integer(pages));
                    state.pc += 1;
                }
                Insn::ParseSchema {
                    db: _,
                    where_clause,
                } => {
                    let conn = self.connection.upgrade();
                    let conn = conn.as_ref().unwrap();
                    let stmt = conn.prepare(format!(
                        "SELECT * FROM  sqlite_schema WHERE {}",
                        where_clause
                    ))?;
                    let mut schema = conn.schema.write();
                    // TODO: This function below is synchronous, make it async
                    parse_schema_rows(
                        Some(stmt),
                        &mut schema,
                        conn.pager.io.clone(),
                        &conn.syms.borrow(),
                        state.mv_tx_id,
                    )?;
                    state.pc += 1;
                }
                Insn::ReadCookie { db, dest, cookie } => {
                    if *db > 0 {
                        // TODO: implement temp databases
                        todo!("temp databases not implemented yet");
                    }
                    let cookie_value = match cookie {
                        Cookie::UserVersion => pager.db_header.lock().user_version.into(),
                        cookie => todo!("{cookie:?} is not yet implement for ReadCookie"),
                    };
                    state.registers[*dest] =
                        Register::OwnedValue(OwnedValue::Integer(cookie_value));
                    state.pc += 1;
                }
                Insn::ShiftRight { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_shift_right(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::ShiftLeft { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_shift_left(
                        state.registers[*lhs].get_owned_value(),
                        state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Variable { index, dest } => {
                    state.registers[*dest] = Register::OwnedValue(
                        state
                            .get_parameter(*index)
                            .ok_or(LimboError::Unbound(*index))?
                            .clone(),
                    );
                    state.pc += 1;
                }
                Insn::ZeroOrNull { rg1, rg2, dest } => {
                    if *state.registers[*rg1].get_owned_value() == OwnedValue::Null
                        || *state.registers[*rg2].get_owned_value() == OwnedValue::Null
                    {
                        state.registers[*dest] = Register::OwnedValue(OwnedValue::Null)
                    } else {
                        state.registers[*dest] = Register::OwnedValue(OwnedValue::Integer(0));
                    }
                    state.pc += 1;
                }
                Insn::Not { reg, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_boolean_not(
                        state.registers[*reg].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Concat { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_concat(
                        &state.registers[*lhs].get_owned_value(),
                        &state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::And { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_and(
                        &state.registers[*lhs].get_owned_value(),
                        &state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Or { lhs, rhs, dest } => {
                    state.registers[*dest] = Register::OwnedValue(exec_or(
                        &state.registers[*lhs].get_owned_value(),
                        &state.registers[*rhs].get_owned_value(),
                    ));
                    state.pc += 1;
                }
                Insn::Noop => {
                    // Do nothing
                    // Advance the program counter for the next opcode
                    state.pc += 1
                }
            }
        }
    }

    fn halt(
        &self,
        pager: Rc<Pager>,
        program_state: &mut ProgramState,
        mv_store: Option<Rc<MvStore>>,
    ) -> Result<StepResult> {
        if let Some(mv_store) = mv_store {
            let conn = self.connection.upgrade().unwrap();
            let auto_commit = *conn.auto_commit.borrow();
            if auto_commit {
                let mut mv_transactions = conn.mv_transactions.borrow_mut();
                for tx_id in mv_transactions.iter() {
                    mv_store.commit_tx(*tx_id).unwrap();
                }
                mv_transactions.clear();
            }
            return Ok(StepResult::Done);
        } else {
            let connection = self
                .connection
                .upgrade()
                .expect("only weak ref to connection?");
            let auto_commit = *connection.auto_commit.borrow();
            tracing::trace!("Halt auto_commit {}", auto_commit);
            assert!(
                program_state.halt_state.is_none()
                    || (matches!(program_state.halt_state.unwrap(), HaltState::Checkpointing))
            );
            if program_state.halt_state.is_some() {
                self.step_end_write_txn(&pager, &mut program_state.halt_state, connection.deref())
            } else {
                if auto_commit {
                    let current_state = connection.transaction_state.borrow().clone();
                    match current_state {
                        TransactionState::Write => self.step_end_write_txn(
                            &pager,
                            &mut program_state.halt_state,
                            connection.deref(),
                        ),
                        TransactionState::Read => {
                            connection.transaction_state.replace(TransactionState::None);
                            pager.end_read_tx()?;
                            Ok(StepResult::Done)
                        }
                        TransactionState::None => Ok(StepResult::Done),
                    }
                } else {
                    if self.change_cnt_on {
                        if let Some(conn) = self.connection.upgrade() {
                            conn.set_changes(self.n_change.get());
                        }
                    }
                    Ok(StepResult::Done)
                }
            }
        }
    }

    fn step_end_write_txn(
        &self,
        pager: &Rc<Pager>,
        halt_state: &mut Option<HaltState>,
        connection: &Connection,
    ) -> Result<StepResult> {
        let checkpoint_status = pager.end_tx()?;
        match checkpoint_status {
            CheckpointStatus::Done(_) => {
                if self.change_cnt_on {
                    if let Some(conn) = self.connection.upgrade() {
                        conn.set_changes(self.n_change.get());
                    }
                }
                connection.transaction_state.replace(TransactionState::None);
                let _ = halt_state.take();
            }
            CheckpointStatus::IO => {
                tracing::trace!("Checkpointing IO");
                *halt_state = Some(HaltState::Checkpointing);
                return Ok(StepResult::IO);
            }
        }
        Ok(StepResult::Done)
    }
}

fn get_new_rowid<R: Rng>(cursor: &mut BTreeCursor, mut rng: R) -> Result<CursorResult<i64>> {
    match cursor.seek_to_last()? {
        CursorResult::Ok(()) => {}
        CursorResult::IO => return Ok(CursorResult::IO),
    }
    let mut rowid = cursor
        .rowid()?
        .unwrap_or(0) // if BTree is empty - use 0 as initial value for rowid
        .checked_add(1) // add 1 but be careful with overflows
        .unwrap_or(u64::MAX); // in case of overflow - use u64::MAX
    if rowid > i64::MAX.try_into().unwrap() {
        let distribution = Uniform::from(1..=i64::MAX);
        let max_attempts = 100;
        for count in 0..max_attempts {
            rowid = distribution.sample(&mut rng).try_into().unwrap();
            match cursor.seek(SeekKey::TableRowId(rowid), SeekOp::EQ)? {
                CursorResult::Ok(false) => break, // Found a non-existing rowid
                CursorResult::Ok(true) => {
                    if count == max_attempts - 1 {
                        return Err(LimboError::InternalError(
                            "Failed to generate a new rowid".to_string(),
                        ));
                    } else {
                        continue; // Try next random rowid
                    }
                }
                CursorResult::IO => return Ok(CursorResult::IO),
            }
        }
    }
    Ok(CursorResult::Ok(rowid.try_into().unwrap()))
}

fn make_owned_record(registers: &[Register], start_reg: &usize, count: &usize) -> Record {
    let mut values = Vec::with_capacity(*count);
    for r in registers.iter().skip(*start_reg).take(*count) {
        values.push(r.get_owned_value().clone())
    }
    Record::new(values)
}

fn trace_insn(program: &Program, addr: InsnReference, insn: &Insn) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    tracing::trace!(
        "{}",
        explain::insn_to_str(
            program,
            addr,
            insn,
            String::new(),
            program
                .comments
                .as_ref()
                .and_then(|comments| comments.get(&{ addr }).copied())
        )
    );
}

fn print_insn(program: &Program, addr: InsnReference, insn: &Insn, indent: String, w: &mut String) {
    let s = explain::insn_to_str(
        program,
        addr,
        insn,
        indent,
        program
            .comments
            .as_ref()
            .and_then(|comments| comments.get(&{ addr }).copied()),
    );
    w.push_str(&s);
}

fn get_indent_count(indent_count: usize, curr_insn: &Insn, prev_insn: Option<&Insn>) -> usize {
    let indent_count = if let Some(insn) = prev_insn {
        match insn {
            Insn::RewindAwait { .. }
            | Insn::LastAwait { .. }
            | Insn::SorterSort { .. }
            | Insn::SeekGE { .. }
            | Insn::SeekGT { .. } => indent_count + 1,
            _ => indent_count,
        }
    } else {
        indent_count
    };

    match curr_insn {
        Insn::NextAsync { .. } | Insn::SorterNext { .. } | Insn::PrevAsync { .. } => {
            indent_count - 1
        }
        _ => indent_count,
    }
}

fn exec_lower(reg: &OwnedValue) -> Option<OwnedValue> {
    match reg {
        OwnedValue::Text(t) => Some(OwnedValue::build_text(&t.as_str().to_lowercase())),
        t => Some(t.to_owned()),
    }
}

fn exec_length(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_) => {
            OwnedValue::Integer(reg.to_string().chars().count() as i64)
        }
        OwnedValue::Blob(blob) => OwnedValue::Integer(blob.len() as i64),
        _ => reg.to_owned(),
    }
}

fn exec_octet_length(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_) => {
            OwnedValue::Integer(reg.to_string().into_bytes().len() as i64)
        }
        OwnedValue::Blob(blob) => OwnedValue::Integer(blob.len() as i64),
        _ => reg.to_owned(),
    }
}

fn exec_upper(reg: &OwnedValue) -> Option<OwnedValue> {
    match reg {
        OwnedValue::Text(t) => Some(OwnedValue::build_text(&t.as_str().to_uppercase())),
        t => Some(t.to_owned()),
    }
}

fn exec_concat_strings(registers: &[Register]) -> OwnedValue {
    let mut result = String::new();
    for reg in registers {
        match reg.get_owned_value() {
            OwnedValue::Null => continue,
            OwnedValue::Blob(_) => todo!("TODO concat blob"),
            v => result.push_str(&format!("{}", v)),
        }
    }
    OwnedValue::build_text(&result)
}

fn exec_concat_ws(registers: &[Register]) -> OwnedValue {
    if registers.is_empty() {
        return OwnedValue::Null;
    }

    let separator = match &registers[0].get_owned_value() {
        OwnedValue::Null | OwnedValue::Blob(_) => return OwnedValue::Null,
        v => format!("{}", v),
    };

    let mut result = String::new();
    for (i, reg) in registers.iter().enumerate().skip(1) {
        if i > 1 {
            result.push_str(&separator);
        }
        match reg.get_owned_value() {
            v if matches!(
                v,
                OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_)
            ) =>
            {
                result.push_str(&format!("{}", v))
            }
            _ => continue,
        }
    }

    OwnedValue::build_text(&result)
}

fn exec_sign(reg: &OwnedValue) -> Option<OwnedValue> {
    let num = match reg {
        OwnedValue::Integer(i) => *i as f64,
        OwnedValue::Float(f) => *f,
        OwnedValue::Text(s) => {
            if let Ok(i) = s.as_str().parse::<i64>() {
                i as f64
            } else if let Ok(f) = s.as_str().parse::<f64>() {
                f
            } else {
                return Some(OwnedValue::Null);
            }
        }
        OwnedValue::Blob(b) => match std::str::from_utf8(b) {
            Ok(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    i as f64
                } else if let Ok(f) = s.parse::<f64>() {
                    f
                } else {
                    return Some(OwnedValue::Null);
                }
            }
            Err(_) => return Some(OwnedValue::Null),
        },
        _ => return Some(OwnedValue::Null),
    };

    let sign = if num > 0.0 {
        1
    } else if num < 0.0 {
        -1
    } else {
        0
    };

    Some(OwnedValue::Integer(sign))
}

/// Generates the Soundex code for a given word
pub fn exec_soundex(reg: &OwnedValue) -> OwnedValue {
    let s = match reg {
        OwnedValue::Null => return OwnedValue::build_text("?000"),
        OwnedValue::Text(s) => {
            // return ?000 if non ASCII alphabet character is found
            if !s.as_str().chars().all(|c| c.is_ascii_alphabetic()) {
                return OwnedValue::build_text("?000");
            }
            s.clone()
        }
        _ => return OwnedValue::build_text("?000"), // For unsupported types, return NULL
    };

    // Remove numbers and spaces
    let word: String = s
        .as_str()
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .collect::<String>()
        .replace(" ", "");
    if word.is_empty() {
        return OwnedValue::build_text("0000");
    }

    let soundex_code = |c| match c {
        'b' | 'f' | 'p' | 'v' => Some('1'),
        'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => Some('2'),
        'd' | 't' => Some('3'),
        'l' => Some('4'),
        'm' | 'n' => Some('5'),
        'r' => Some('6'),
        _ => None,
    };

    // Convert the word to lowercase for consistent lookups
    let word = word.to_lowercase();
    let first_letter = word.chars().next().unwrap();

    // Remove all occurrences of 'h' and 'w' except the first letter
    let code: String = word
        .chars()
        .skip(1)
        .filter(|&ch| ch != 'h' && ch != 'w')
        .fold(first_letter.to_string(), |mut acc, ch| {
            acc.push(ch);
            acc
        });

    // Replace consonants with digits based on Soundex mapping
    let tmp: String = code
        .chars()
        .map(|ch| match soundex_code(ch) {
            Some(code) => code.to_string(),
            None => ch.to_string(),
        })
        .collect();

    // Remove adjacent same digits
    let tmp = tmp.chars().fold(String::new(), |mut acc, ch| {
        if !acc.ends_with(ch) {
            acc.push(ch);
        }
        acc
    });

    // Remove all occurrences of a, e, i, o, u, y except the first letter
    let mut result = tmp
        .chars()
        .enumerate()
        .filter(|(i, ch)| *i == 0 || !matches!(ch, 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
        .map(|(_, ch)| ch)
        .collect::<String>();

    // If the first symbol is a digit, replace it with the saved first letter
    if let Some(first_digit) = result.chars().next() {
        if first_digit.is_ascii_digit() {
            result.replace_range(0..1, &first_letter.to_string());
        }
    }

    // Append zeros if the result contains less than 4 characters
    while result.len() < 4 {
        result.push('0');
    }

    // Retain the first 4 characters and convert to uppercase
    result.truncate(4);
    OwnedValue::build_text(&result.to_uppercase())
}

fn exec_abs(reg: &OwnedValue) -> Result<OwnedValue> {
    match reg {
        OwnedValue::Integer(x) => {
            match i64::checked_abs(*x) {
                Some(y) => Ok(OwnedValue::Integer(y)),
                // Special case: if we do the abs of "-9223372036854775808", it causes overflow.
                // return IntegerOverflow error
                None => Err(LimboError::IntegerOverflow),
            }
        }
        OwnedValue::Float(x) => {
            if x < &0.0 {
                Ok(OwnedValue::Float(-x))
            } else {
                Ok(OwnedValue::Float(*x))
            }
        }
        OwnedValue::Null => Ok(OwnedValue::Null),
        _ => Ok(OwnedValue::Float(0.0)),
    }
}

fn exec_random() -> OwnedValue {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).unwrap();
    let random_number = i64::from_ne_bytes(buf);
    OwnedValue::Integer(random_number)
}

fn exec_randomblob(reg: &OwnedValue) -> OwnedValue {
    let length = match reg {
        OwnedValue::Integer(i) => *i,
        OwnedValue::Float(f) => *f as i64,
        OwnedValue::Text(t) => t.as_str().parse().unwrap_or(1),
        _ => 1,
    }
    .max(1) as usize;

    let mut blob: Vec<u8> = vec![0; length];
    getrandom::getrandom(&mut blob).expect("Failed to generate random blob");
    OwnedValue::Blob(Rc::new(blob))
}

fn exec_quote(value: &OwnedValue) -> OwnedValue {
    match value {
        OwnedValue::Null => OwnedValue::build_text("NULL"),
        OwnedValue::Integer(_) | OwnedValue::Float(_) => value.to_owned(),
        OwnedValue::Blob(_) => todo!(),
        OwnedValue::Text(s) => {
            let mut quoted = String::with_capacity(s.as_str().len() + 2);
            quoted.push('\'');
            for c in s.as_str().chars() {
                if c == '\0' {
                    break;
                } else if c == '\'' {
                    quoted.push('\'');
                    quoted.push(c);
                } else {
                    quoted.push(c);
                }
            }
            quoted.push('\'');
            OwnedValue::build_text(&quoted)
        }
    }
}

fn exec_char(values: &[Register]) -> OwnedValue {
    let result: String = values
        .iter()
        .filter_map(|x| {
            if let OwnedValue::Integer(i) = x.get_owned_value() {
                Some(*i as u8 as char)
            } else {
                None
            }
        })
        .collect();
    OwnedValue::build_text(&result)
}

fn construct_like_regex(pattern: &str) -> Regex {
    let mut regex_pattern = String::with_capacity(pattern.len() * 2);

    regex_pattern.push('^');

    for c in pattern.chars() {
        match c {
            '\\' => regex_pattern.push_str("\\\\"),
            '%' => regex_pattern.push_str(".*"),
            '_' => regex_pattern.push('.'),
            ch => {
                if regex_syntax::is_meta_character(c) {
                    regex_pattern.push('\\');
                }
                regex_pattern.push(ch);
            }
        }
    }

    regex_pattern.push('$');

    RegexBuilder::new(&regex_pattern)
        .case_insensitive(true)
        .dot_matches_new_line(true)
        .build()
        .unwrap()
}

// Implements LIKE pattern matching. Caches the constructed regex if a cache is provided
fn exec_like(regex_cache: Option<&mut HashMap<String, Regex>>, pattern: &str, text: &str) -> bool {
    if let Some(cache) = regex_cache {
        match cache.get(pattern) {
            Some(re) => re.is_match(text),
            None => {
                let re = construct_like_regex(pattern);
                let res = re.is_match(text);
                cache.insert(pattern.to_string(), re);
                res
            }
        }
    } else {
        let re = construct_like_regex(pattern);
        re.is_match(text)
    }
}

fn exec_min(regs: &[Register]) -> OwnedValue {
    regs.iter()
        .map(|v| v.get_owned_value())
        .min()
        .map(|v| v.to_owned())
        .unwrap_or(OwnedValue::Null)
}

fn exec_max(regs: &[Register]) -> OwnedValue {
    regs.iter()
        .map(|v| v.get_owned_value())
        .max()
        .map(|v| v.to_owned())
        .unwrap_or(OwnedValue::Null)
}

fn exec_nullif(first_value: &OwnedValue, second_value: &OwnedValue) -> OwnedValue {
    if first_value != second_value {
        first_value.clone()
    } else {
        OwnedValue::Null
    }
}

fn exec_substring(
    str_value: &OwnedValue,
    start_value: &OwnedValue,
    length_value: Option<&OwnedValue>,
) -> OwnedValue {
    if let (OwnedValue::Text(str), OwnedValue::Integer(start)) = (str_value, start_value) {
        let str_len = str.as_str().len() as i64;

        // The left-most character of X is number 1.
        // If Y is negative then the first character of the substring is found by counting from the right rather than the left.
        let first_position = if *start < 0 {
            str_len.saturating_sub((*start).abs())
        } else {
            *start - 1
        };
        // If Z is negative then the abs(Z) characters preceding the Y-th character are returned.
        let last_position = match length_value {
            Some(OwnedValue::Integer(length)) => first_position + *length,
            _ => str_len,
        };
        let (start, end) = if first_position <= last_position {
            (first_position, last_position)
        } else {
            (last_position, first_position)
        };
        OwnedValue::build_text(
            &str.as_str()[start.clamp(-0, str_len) as usize..end.clamp(0, str_len) as usize],
        )
    } else {
        OwnedValue::Null
    }
}

fn exec_instr(reg: &OwnedValue, pattern: &OwnedValue) -> OwnedValue {
    if reg == &OwnedValue::Null || pattern == &OwnedValue::Null {
        return OwnedValue::Null;
    }

    if let (OwnedValue::Blob(reg), OwnedValue::Blob(pattern)) = (reg, pattern) {
        let result = reg
            .windows(pattern.len())
            .position(|window| window == **pattern)
            .map_or(0, |i| i + 1);
        return OwnedValue::Integer(result as i64);
    }

    let reg_str;
    let reg = match reg {
        OwnedValue::Text(s) => s.as_str(),
        _ => {
            reg_str = reg.to_string();
            reg_str.as_str()
        }
    };

    let pattern_str;
    let pattern = match pattern {
        OwnedValue::Text(s) => s.as_str(),
        _ => {
            pattern_str = pattern.to_string();
            pattern_str.as_str()
        }
    };

    match reg.find(pattern) {
        Some(position) => OwnedValue::Integer(position as i64 + 1),
        None => OwnedValue::Integer(0),
    }
}

fn exec_typeof(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Null => OwnedValue::build_text("null"),
        OwnedValue::Integer(_) => OwnedValue::build_text("integer"),
        OwnedValue::Float(_) => OwnedValue::build_text("real"),
        OwnedValue::Text(_) => OwnedValue::build_text("text"),
        OwnedValue::Blob(_) => OwnedValue::build_text("blob"),
    }
}

fn exec_hex(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Text(_)
        | OwnedValue::Integer(_)
        | OwnedValue::Float(_)
        | OwnedValue::Blob(_) => {
            let text = reg.to_string();
            OwnedValue::build_text(&hex::encode_upper(text))
        }
        _ => OwnedValue::Null,
    }
}

fn exec_unhex(reg: &OwnedValue, ignored_chars: Option<&OwnedValue>) -> OwnedValue {
    match reg {
        OwnedValue::Null => OwnedValue::Null,
        _ => match ignored_chars {
            None => match hex::decode(reg.to_string()) {
                Ok(bytes) => OwnedValue::Blob(Rc::new(bytes)),
                Err(_) => OwnedValue::Null,
            },
            Some(ignore) => match ignore {
                OwnedValue::Text(_) => {
                    let pat = ignore.to_string();
                    let trimmed = reg
                        .to_string()
                        .trim_start_matches(|x| pat.contains(x))
                        .trim_end_matches(|x| pat.contains(x))
                        .to_string();
                    match hex::decode(trimmed) {
                        Ok(bytes) => OwnedValue::Blob(Rc::new(bytes)),
                        Err(_) => OwnedValue::Null,
                    }
                }
                _ => OwnedValue::Null,
            },
        },
    }
}

fn exec_unicode(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Text(_)
        | OwnedValue::Integer(_)
        | OwnedValue::Float(_)
        | OwnedValue::Blob(_) => {
            let text = reg.to_string();
            if let Some(first_char) = text.chars().next() {
                OwnedValue::Integer(first_char as u32 as i64)
            } else {
                OwnedValue::Null
            }
        }
        _ => OwnedValue::Null,
    }
}

fn _to_float(reg: &OwnedValue) -> f64 {
    match reg {
        OwnedValue::Text(x) => match cast_text_to_numeric(x.as_str()) {
            OwnedValue::Integer(i) => i as f64,
            OwnedValue::Float(f) => f,
            _ => unreachable!(),
        },
        OwnedValue::Integer(x) => *x as f64,
        OwnedValue::Float(x) => *x,
        _ => 0.0,
    }
}

fn exec_round(reg: &OwnedValue, precision: Option<&OwnedValue>) -> OwnedValue {
    let reg = _to_float(reg);
    let round = |reg: f64, f: f64| {
        let precision = if f < 1.0 { 0.0 } else { f };
        OwnedValue::Float(reg.round_to_precision(precision as i32))
    };
    match precision {
        Some(OwnedValue::Text(x)) => match cast_text_to_numeric(x.as_str()) {
            OwnedValue::Integer(i) => round(reg, i as f64),
            OwnedValue::Float(f) => round(reg, f),
            _ => unreachable!(),
        },
        Some(OwnedValue::Integer(i)) => round(reg, *i as f64),
        Some(OwnedValue::Float(f)) => round(reg, *f),
        None => round(reg, 0.0),
        _ => OwnedValue::Null,
    }
}

// Implements TRIM pattern matching.
fn exec_trim(reg: &OwnedValue, pattern: Option<&OwnedValue>) -> OwnedValue {
    match (reg, pattern) {
        (reg, Some(pattern)) => match reg {
            OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_) => {
                let pattern_chars: Vec<char> = pattern.to_string().chars().collect();
                OwnedValue::build_text(reg.to_string().trim_matches(&pattern_chars[..]))
            }
            _ => reg.to_owned(),
        },
        (OwnedValue::Text(t), None) => OwnedValue::build_text(t.as_str().trim()),
        (reg, _) => reg.to_owned(),
    }
}

// Implements LTRIM pattern matching.
fn exec_ltrim(reg: &OwnedValue, pattern: Option<&OwnedValue>) -> OwnedValue {
    match (reg, pattern) {
        (reg, Some(pattern)) => match reg {
            OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_) => {
                let pattern_chars: Vec<char> = pattern.to_string().chars().collect();
                OwnedValue::build_text(reg.to_string().trim_start_matches(&pattern_chars[..]))
            }
            _ => reg.to_owned(),
        },
        (OwnedValue::Text(t), None) => OwnedValue::build_text(t.as_str().trim_start()),
        (reg, _) => reg.to_owned(),
    }
}

// Implements RTRIM pattern matching.
fn exec_rtrim(reg: &OwnedValue, pattern: Option<&OwnedValue>) -> OwnedValue {
    match (reg, pattern) {
        (reg, Some(pattern)) => match reg {
            OwnedValue::Text(_) | OwnedValue::Integer(_) | OwnedValue::Float(_) => {
                let pattern_chars: Vec<char> = pattern.to_string().chars().collect();
                OwnedValue::build_text(reg.to_string().trim_end_matches(&pattern_chars[..]))
            }
            _ => reg.to_owned(),
        },
        (OwnedValue::Text(t), None) => OwnedValue::build_text(t.as_str().trim_end()),
        (reg, _) => reg.to_owned(),
    }
}

fn exec_zeroblob(req: &OwnedValue) -> OwnedValue {
    let length: i64 = match req {
        OwnedValue::Integer(i) => *i,
        OwnedValue::Float(f) => *f as i64,
        OwnedValue::Text(s) => s.as_str().parse().unwrap_or(0),
        _ => 0,
    };
    OwnedValue::Blob(Rc::new(vec![0; length.max(0) as usize]))
}

// exec_if returns whether you should jump
fn exec_if(reg: &OwnedValue, jump_if_null: bool, not: bool) -> bool {
    match reg {
        OwnedValue::Integer(0) | OwnedValue::Float(0.0) => not,
        OwnedValue::Integer(_) | OwnedValue::Float(_) => !not,
        OwnedValue::Null => jump_if_null,
        _ => false,
    }
}

fn exec_cast(value: &OwnedValue, datatype: &str) -> OwnedValue {
    if matches!(value, OwnedValue::Null) {
        return OwnedValue::Null;
    }
    match affinity(datatype) {
        // NONE	Casting a value to a type-name with no affinity causes the value to be converted into a BLOB. Casting to a BLOB consists of first casting the value to TEXT in the encoding of the database connection, then interpreting the resulting byte sequence as a BLOB instead of as TEXT.
        // Historically called NONE, but it's the same as BLOB
        Affinity::Blob => {
            // Convert to TEXT first, then interpret as BLOB
            // TODO: handle encoding
            let text = value.to_string();
            OwnedValue::Blob(Rc::new(text.into_bytes()))
        }
        // TEXT To cast a BLOB value to TEXT, the sequence of bytes that make up the BLOB is interpreted as text encoded using the database encoding.
        // Casting an INTEGER or REAL value into TEXT renders the value as if via sqlite3_snprintf() except that the resulting TEXT uses the encoding of the database connection.
        Affinity::Text => {
            // Convert everything to text representation
            // TODO: handle encoding and whatever sqlite3_snprintf does
            OwnedValue::build_text(&value.to_string())
        }
        Affinity::Real => match value {
            OwnedValue::Blob(b) => {
                // Convert BLOB to TEXT first
                let text = String::from_utf8_lossy(b);
                cast_text_to_real(&text)
            }
            OwnedValue::Text(t) => cast_text_to_real(t.as_str()),
            OwnedValue::Integer(i) => OwnedValue::Float(*i as f64),
            OwnedValue::Float(f) => OwnedValue::Float(*f),
            _ => OwnedValue::Float(0.0),
        },
        Affinity::Integer => match value {
            OwnedValue::Blob(b) => {
                // Convert BLOB to TEXT first
                let text = String::from_utf8_lossy(b);
                cast_text_to_integer(&text)
            }
            OwnedValue::Text(t) => cast_text_to_integer(t.as_str()),
            OwnedValue::Integer(i) => OwnedValue::Integer(*i),
            // A cast of a REAL value into an INTEGER results in the integer between the REAL value and zero
            // that is closest to the REAL value. If a REAL is greater than the greatest possible signed integer (+9223372036854775807)
            // then the result is the greatest possible signed integer and if the REAL is less than the least possible signed integer (-9223372036854775808)
            // then the result is the least possible signed integer.
            OwnedValue::Float(f) => {
                let i = f.trunc() as i128;
                if i > i64::MAX as i128 {
                    OwnedValue::Integer(i64::MAX)
                } else if i < i64::MIN as i128 {
                    OwnedValue::Integer(i64::MIN)
                } else {
                    OwnedValue::Integer(i as i64)
                }
            }
            _ => OwnedValue::Integer(0),
        },
        Affinity::Numeric => match value {
            OwnedValue::Blob(b) => {
                let text = String::from_utf8_lossy(b);
                cast_text_to_numeric(&text)
            }
            OwnedValue::Text(t) => cast_text_to_numeric(t.as_str()),
            OwnedValue::Integer(i) => OwnedValue::Integer(*i),
            OwnedValue::Float(f) => OwnedValue::Float(*f),
            _ => value.clone(), // TODO probably wrong
        },
    }
}

fn exec_replace(source: &OwnedValue, pattern: &OwnedValue, replacement: &OwnedValue) -> OwnedValue {
    // The replace(X,Y,Z) function returns a string formed by substituting string Z for every occurrence of
    // string Y in string X. The BINARY collating sequence is used for comparisons. If Y is an empty string
    // then return X unchanged. If Z is not initially a string, it is cast to a UTF-8 string prior to processing.

    // If any of the arguments is NULL, the result is NULL.
    if matches!(source, OwnedValue::Null)
        || matches!(pattern, OwnedValue::Null)
        || matches!(replacement, OwnedValue::Null)
    {
        return OwnedValue::Null;
    }

    let source = exec_cast(source, "TEXT");
    let pattern = exec_cast(pattern, "TEXT");
    let replacement = exec_cast(replacement, "TEXT");

    // If any of the casts failed, panic as text casting is not expected to fail.
    match (&source, &pattern, &replacement) {
        (OwnedValue::Text(source), OwnedValue::Text(pattern), OwnedValue::Text(replacement)) => {
            if pattern.as_str().is_empty() {
                return OwnedValue::Text(source.clone());
            }

            let result = source
                .as_str()
                .replace(pattern.as_str(), replacement.as_str());
            OwnedValue::build_text(&result)
        }
        _ => unreachable!("text cast should never fail"),
    }
}

fn execute_sqlite_version(version_integer: i64) -> String {
    let major = version_integer / 1_000_000;
    let minor = (version_integer % 1_000_000) / 1_000;
    let release = version_integer % 1_000;

    format!("{}.{}.{}", major, minor, release)
}

fn to_f64(reg: &OwnedValue) -> Option<f64> {
    match reg {
        OwnedValue::Integer(i) => Some(*i as f64),
        OwnedValue::Float(f) => Some(*f),
        OwnedValue::Text(t) => t.as_str().parse::<f64>().ok(),
        _ => None,
    }
}

fn exec_math_unary(reg: &OwnedValue, function: &MathFunc) -> OwnedValue {
    // In case of some functions and integer input, return the input as is
    if let OwnedValue::Integer(_) = reg {
        if matches! { function, MathFunc::Ceil | MathFunc::Ceiling | MathFunc::Floor | MathFunc::Trunc }
        {
            return reg.clone();
        }
    }

    let f = match to_f64(reg) {
        Some(f) => f,
        None => return OwnedValue::Null,
    };

    let result = match function {
        MathFunc::Acos => libm::acos(f),
        MathFunc::Acosh => libm::acosh(f),
        MathFunc::Asin => libm::asin(f),
        MathFunc::Asinh => libm::asinh(f),
        MathFunc::Atan => libm::atan(f),
        MathFunc::Atanh => libm::atanh(f),
        MathFunc::Ceil | MathFunc::Ceiling => libm::ceil(f),
        MathFunc::Cos => libm::cos(f),
        MathFunc::Cosh => libm::cosh(f),
        MathFunc::Degrees => f.to_degrees(),
        MathFunc::Exp => libm::exp(f),
        MathFunc::Floor => libm::floor(f),
        MathFunc::Ln => libm::log(f),
        MathFunc::Log10 => libm::log10(f),
        MathFunc::Log2 => libm::log2(f),
        MathFunc::Radians => f.to_radians(),
        MathFunc::Sin => libm::sin(f),
        MathFunc::Sinh => libm::sinh(f),
        MathFunc::Sqrt => libm::sqrt(f),
        MathFunc::Tan => libm::tan(f),
        MathFunc::Tanh => libm::tanh(f),
        MathFunc::Trunc => libm::trunc(f),
        _ => unreachable!("Unexpected mathematical unary function {:?}", function),
    };

    if result.is_nan() {
        OwnedValue::Null
    } else {
        OwnedValue::Float(result)
    }
}

fn exec_math_binary(lhs: &OwnedValue, rhs: &OwnedValue, function: &MathFunc) -> OwnedValue {
    let lhs = match to_f64(lhs) {
        Some(f) => f,
        None => return OwnedValue::Null,
    };

    let rhs = match to_f64(rhs) {
        Some(f) => f,
        None => return OwnedValue::Null,
    };

    let result = match function {
        MathFunc::Atan2 => libm::atan2(lhs, rhs),
        MathFunc::Mod => libm::fmod(lhs, rhs),
        MathFunc::Pow | MathFunc::Power => libm::pow(lhs, rhs),
        _ => unreachable!("Unexpected mathematical binary function {:?}", function),
    };

    if result.is_nan() {
        OwnedValue::Null
    } else {
        OwnedValue::Float(result)
    }
}

fn exec_math_log(arg: &OwnedValue, base: Option<&OwnedValue>) -> OwnedValue {
    let f = match to_f64(arg) {
        Some(f) => f,
        None => return OwnedValue::Null,
    };

    let base = match base {
        Some(base) => match to_f64(base) {
            Some(f) => f,
            None => return OwnedValue::Null,
        },
        None => 10.0,
    };

    if f <= 0.0 || base <= 0.0 || base == 1.0 {
        return OwnedValue::Null;
    }
    let log_x = libm::log(f);
    let log_base = libm::log(base);
    let result = log_x / log_base;
    OwnedValue::Float(result)
}

#[cfg(test)]
mod tests {
    use crate::vdbe::{exec_replace, Register};

    use super::{
        exec_abs, exec_char, exec_hex, exec_if, exec_instr, exec_length, exec_like, exec_lower,
        exec_ltrim, exec_max, exec_min, exec_nullif, exec_quote, exec_random, exec_randomblob,
        exec_round, exec_rtrim, exec_sign, exec_soundex, exec_substring, exec_trim, exec_typeof,
        exec_unhex, exec_unicode, exec_upper, exec_zeroblob, execute_sqlite_version, Bitfield,
        OwnedValue,
    };
    use std::{collections::HashMap, rc::Rc};

    #[test]
    fn test_length() {
        let input_str = OwnedValue::build_text("bob");
        let expected_len = OwnedValue::Integer(3);
        assert_eq!(exec_length(&input_str), expected_len);

        let input_integer = OwnedValue::Integer(123);
        let expected_len = OwnedValue::Integer(3);
        assert_eq!(exec_length(&input_integer), expected_len);

        let input_float = OwnedValue::Float(123.456);
        let expected_len = OwnedValue::Integer(7);
        assert_eq!(exec_length(&input_float), expected_len);

        let expected_blob = OwnedValue::Blob(Rc::new("example".as_bytes().to_vec()));
        let expected_len = OwnedValue::Integer(7);
        assert_eq!(exec_length(&expected_blob), expected_len);
    }

    #[test]
    fn test_quote() {
        let input = OwnedValue::build_text("abc\0edf");
        let expected = OwnedValue::build_text("'abc'");
        assert_eq!(exec_quote(&input), expected);

        let input = OwnedValue::Integer(123);
        let expected = OwnedValue::Integer(123);
        assert_eq!(exec_quote(&input), expected);

        let input = OwnedValue::build_text("hello''world");
        let expected = OwnedValue::build_text("'hello''''world'");
        assert_eq!(exec_quote(&input), expected);
    }

    #[test]
    fn test_typeof() {
        let input = OwnedValue::Null;
        let expected: OwnedValue = OwnedValue::build_text("null");
        assert_eq!(exec_typeof(&input), expected);

        let input = OwnedValue::Integer(123);
        let expected: OwnedValue = OwnedValue::build_text("integer");
        assert_eq!(exec_typeof(&input), expected);

        let input = OwnedValue::Float(123.456);
        let expected: OwnedValue = OwnedValue::build_text("real");
        assert_eq!(exec_typeof(&input), expected);

        let input = OwnedValue::build_text("hello");
        let expected: OwnedValue = OwnedValue::build_text("text");
        assert_eq!(exec_typeof(&input), expected);

        let input = OwnedValue::Blob(Rc::new("limbo".as_bytes().to_vec()));
        let expected: OwnedValue = OwnedValue::build_text("blob");
        assert_eq!(exec_typeof(&input), expected);
    }

    #[test]
    fn test_unicode() {
        assert_eq!(
            exec_unicode(&OwnedValue::build_text("a")),
            OwnedValue::Integer(97)
        );
        assert_eq!(
            exec_unicode(&OwnedValue::build_text("😊")),
            OwnedValue::Integer(128522)
        );
        assert_eq!(exec_unicode(&OwnedValue::build_text("")), OwnedValue::Null);
        assert_eq!(
            exec_unicode(&OwnedValue::Integer(23)),
            OwnedValue::Integer(50)
        );
        assert_eq!(
            exec_unicode(&OwnedValue::Integer(0)),
            OwnedValue::Integer(48)
        );
        assert_eq!(
            exec_unicode(&OwnedValue::Float(0.0)),
            OwnedValue::Integer(48)
        );
        assert_eq!(
            exec_unicode(&OwnedValue::Float(23.45)),
            OwnedValue::Integer(50)
        );
        assert_eq!(exec_unicode(&OwnedValue::Null), OwnedValue::Null);
        assert_eq!(
            exec_unicode(&OwnedValue::Blob(Rc::new("example".as_bytes().to_vec()))),
            OwnedValue::Integer(101)
        );
    }

    #[test]
    fn test_min_max() {
        let input_int_vec = vec![
            Register::OwnedValue(OwnedValue::Integer(-1)),
            Register::OwnedValue(OwnedValue::Integer(10)),
        ];
        assert_eq!(exec_min(&input_int_vec), OwnedValue::Integer(-1));
        assert_eq!(exec_max(&input_int_vec), OwnedValue::Integer(10));

        let str1 = Register::OwnedValue(OwnedValue::build_text("A"));
        let str2 = Register::OwnedValue(OwnedValue::build_text("z"));
        let input_str_vec = vec![str2, str1.clone()];
        assert_eq!(exec_min(&input_str_vec), OwnedValue::build_text("A"));
        assert_eq!(exec_max(&input_str_vec), OwnedValue::build_text("z"));

        let input_null_vec = vec![
            Register::OwnedValue(OwnedValue::Null),
            Register::OwnedValue(OwnedValue::Null),
        ];
        assert_eq!(exec_min(&input_null_vec), OwnedValue::Null);
        assert_eq!(exec_max(&input_null_vec), OwnedValue::Null);

        let input_mixed_vec = vec![Register::OwnedValue(OwnedValue::Integer(10)), str1];
        assert_eq!(exec_min(&input_mixed_vec), OwnedValue::Integer(10));
        assert_eq!(exec_max(&input_mixed_vec), OwnedValue::build_text("A"));
    }

    #[test]
    fn test_trim() {
        let input_str = OwnedValue::build_text("     Bob and Alice     ");
        let expected_str = OwnedValue::build_text("Bob and Alice");
        assert_eq!(exec_trim(&input_str, None), expected_str);

        let input_str = OwnedValue::build_text("     Bob and Alice     ");
        let pattern_str = OwnedValue::build_text("Bob and");
        let expected_str = OwnedValue::build_text("Alice");
        assert_eq!(exec_trim(&input_str, Some(&pattern_str)), expected_str);
    }

    #[test]
    fn test_ltrim() {
        let input_str = OwnedValue::build_text("     Bob and Alice     ");
        let expected_str = OwnedValue::build_text("Bob and Alice     ");
        assert_eq!(exec_ltrim(&input_str, None), expected_str);

        let input_str = OwnedValue::build_text("     Bob and Alice     ");
        let pattern_str = OwnedValue::build_text("Bob and");
        let expected_str = OwnedValue::build_text("Alice     ");
        assert_eq!(exec_ltrim(&input_str, Some(&pattern_str)), expected_str);
    }

    #[test]
    fn test_rtrim() {
        let input_str = OwnedValue::build_text("     Bob and Alice     ");
        let expected_str = OwnedValue::build_text("     Bob and Alice");
        assert_eq!(exec_rtrim(&input_str, None), expected_str);

        let input_str = OwnedValue::build_text("     Bob and Alice     ");
        let pattern_str = OwnedValue::build_text("Bob and");
        let expected_str = OwnedValue::build_text("     Bob and Alice");
        assert_eq!(exec_rtrim(&input_str, Some(&pattern_str)), expected_str);

        let input_str = OwnedValue::build_text("     Bob and Alice     ");
        let pattern_str = OwnedValue::build_text("and Alice");
        let expected_str = OwnedValue::build_text("     Bob");
        assert_eq!(exec_rtrim(&input_str, Some(&pattern_str)), expected_str);
    }

    #[test]
    fn test_soundex() {
        let input_str = OwnedValue::build_text("Pfister");
        let expected_str = OwnedValue::build_text("P236");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("husobee");
        let expected_str = OwnedValue::build_text("H210");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("Tymczak");
        let expected_str = OwnedValue::build_text("T522");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("Ashcraft");
        let expected_str = OwnedValue::build_text("A261");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("Robert");
        let expected_str = OwnedValue::build_text("R163");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("Rupert");
        let expected_str = OwnedValue::build_text("R163");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("Rubin");
        let expected_str = OwnedValue::build_text("R150");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("Kant");
        let expected_str = OwnedValue::build_text("K530");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("Knuth");
        let expected_str = OwnedValue::build_text("K530");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("x");
        let expected_str = OwnedValue::build_text("X000");
        assert_eq!(exec_soundex(&input_str), expected_str);

        let input_str = OwnedValue::build_text("闪电五连鞭");
        let expected_str = OwnedValue::build_text("?000");
        assert_eq!(exec_soundex(&input_str), expected_str);
    }

    #[test]
    fn test_upper_case() {
        let input_str = OwnedValue::build_text("Limbo");
        let expected_str = OwnedValue::build_text("LIMBO");
        assert_eq!(exec_upper(&input_str).unwrap(), expected_str);

        let input_int = OwnedValue::Integer(10);
        assert_eq!(exec_upper(&input_int).unwrap(), input_int);
        assert_eq!(exec_upper(&OwnedValue::Null).unwrap(), OwnedValue::Null)
    }

    #[test]
    fn test_lower_case() {
        let input_str = OwnedValue::build_text("Limbo");
        let expected_str = OwnedValue::build_text("limbo");
        assert_eq!(exec_lower(&input_str).unwrap(), expected_str);

        let input_int = OwnedValue::Integer(10);
        assert_eq!(exec_lower(&input_int).unwrap(), input_int);
        assert_eq!(exec_lower(&OwnedValue::Null).unwrap(), OwnedValue::Null)
    }

    #[test]
    fn test_hex() {
        let input_str = OwnedValue::build_text("limbo");
        let expected_val = OwnedValue::build_text("6C696D626F");
        assert_eq!(exec_hex(&input_str), expected_val);

        let input_int = OwnedValue::Integer(100);
        let expected_val = OwnedValue::build_text("313030");
        assert_eq!(exec_hex(&input_int), expected_val);

        let input_float = OwnedValue::Float(12.34);
        let expected_val = OwnedValue::build_text("31322E3334");
        assert_eq!(exec_hex(&input_float), expected_val);
    }

    #[test]
    fn test_unhex() {
        let input = OwnedValue::build_text("6f");
        let expected = OwnedValue::Blob(Rc::new(vec![0x6f]));
        assert_eq!(exec_unhex(&input, None), expected);

        let input = OwnedValue::build_text("6f");
        let expected = OwnedValue::Blob(Rc::new(vec![0x6f]));
        assert_eq!(exec_unhex(&input, None), expected);

        let input = OwnedValue::build_text("611");
        let expected = OwnedValue::Null;
        assert_eq!(exec_unhex(&input, None), expected);

        let input = OwnedValue::build_text("");
        let expected = OwnedValue::Blob(Rc::new(vec![]));
        assert_eq!(exec_unhex(&input, None), expected);

        let input = OwnedValue::build_text("61x");
        let expected = OwnedValue::Null;
        assert_eq!(exec_unhex(&input, None), expected);

        let input = OwnedValue::Null;
        let expected = OwnedValue::Null;
        assert_eq!(exec_unhex(&input, None), expected);
    }

    #[test]
    fn test_abs() {
        let int_positive_reg = OwnedValue::Integer(10);
        let int_negative_reg = OwnedValue::Integer(-10);
        assert_eq!(exec_abs(&int_positive_reg).unwrap(), int_positive_reg);
        assert_eq!(exec_abs(&int_negative_reg).unwrap(), int_positive_reg);

        let float_positive_reg = OwnedValue::Integer(10);
        let float_negative_reg = OwnedValue::Integer(-10);
        assert_eq!(exec_abs(&float_positive_reg).unwrap(), float_positive_reg);
        assert_eq!(exec_abs(&float_negative_reg).unwrap(), float_positive_reg);

        assert_eq!(
            exec_abs(&OwnedValue::build_text("a")).unwrap(),
            OwnedValue::Float(0.0)
        );
        assert_eq!(exec_abs(&OwnedValue::Null).unwrap(), OwnedValue::Null);

        // ABS(i64::MIN) should return RuntimeError
        assert!(exec_abs(&OwnedValue::Integer(i64::MIN)).is_err());
    }

    #[test]
    fn test_char() {
        assert_eq!(
            exec_char(&[
                Register::OwnedValue(OwnedValue::Integer(108)),
                Register::OwnedValue(OwnedValue::Integer(105))
            ]),
            OwnedValue::build_text("li")
        );
        assert_eq!(exec_char(&[]), OwnedValue::build_text(""));
        assert_eq!(
            exec_char(&[Register::OwnedValue(OwnedValue::Null)]),
            OwnedValue::build_text("")
        );
        assert_eq!(
            exec_char(&[Register::OwnedValue(OwnedValue::build_text("a"))]),
            OwnedValue::build_text("")
        );
    }

    #[test]
    fn test_like_with_escape_or_regexmeta_chars() {
        assert!(exec_like(None, r#"\%A"#, r#"\A"#));
        assert!(exec_like(None, "%a%a", "aaaa"));
    }

    #[test]
    fn test_like_no_cache() {
        assert!(exec_like(None, "a%", "aaaa"));
        assert!(exec_like(None, "%a%a", "aaaa"));
        assert!(!exec_like(None, "%a.a", "aaaa"));
        assert!(!exec_like(None, "a.a%", "aaaa"));
        assert!(!exec_like(None, "%a.ab", "aaaa"));
    }

    #[test]
    fn test_like_with_cache() {
        let mut cache = HashMap::new();
        assert!(exec_like(Some(&mut cache), "a%", "aaaa"));
        assert!(exec_like(Some(&mut cache), "%a%a", "aaaa"));
        assert!(!exec_like(Some(&mut cache), "%a.a", "aaaa"));
        assert!(!exec_like(Some(&mut cache), "a.a%", "aaaa"));
        assert!(!exec_like(Some(&mut cache), "%a.ab", "aaaa"));

        // again after values have been cached
        assert!(exec_like(Some(&mut cache), "a%", "aaaa"));
        assert!(exec_like(Some(&mut cache), "%a%a", "aaaa"));
        assert!(!exec_like(Some(&mut cache), "%a.a", "aaaa"));
        assert!(!exec_like(Some(&mut cache), "a.a%", "aaaa"));
        assert!(!exec_like(Some(&mut cache), "%a.ab", "aaaa"));
    }

    #[test]
    fn test_random() {
        match exec_random() {
            OwnedValue::Integer(value) => {
                // Check that the value is within the range of i64
                assert!(
                    (i64::MIN..=i64::MAX).contains(&value),
                    "Random number out of range"
                );
            }
            _ => panic!("exec_random did not return an Integer variant"),
        }
    }

    #[test]
    fn test_exec_randomblob() {
        struct TestCase {
            input: OwnedValue,
            expected_len: usize,
        }

        let test_cases = vec![
            TestCase {
                input: OwnedValue::Integer(5),
                expected_len: 5,
            },
            TestCase {
                input: OwnedValue::Integer(0),
                expected_len: 1,
            },
            TestCase {
                input: OwnedValue::Integer(-1),
                expected_len: 1,
            },
            TestCase {
                input: OwnedValue::build_text(""),
                expected_len: 1,
            },
            TestCase {
                input: OwnedValue::build_text("5"),
                expected_len: 5,
            },
            TestCase {
                input: OwnedValue::build_text("0"),
                expected_len: 1,
            },
            TestCase {
                input: OwnedValue::build_text("-1"),
                expected_len: 1,
            },
            TestCase {
                input: OwnedValue::Float(2.9),
                expected_len: 2,
            },
            TestCase {
                input: OwnedValue::Float(-3.15),
                expected_len: 1,
            },
            TestCase {
                input: OwnedValue::Null,
                expected_len: 1,
            },
        ];

        for test_case in &test_cases {
            let result = exec_randomblob(&test_case.input);
            match result {
                OwnedValue::Blob(blob) => {
                    assert_eq!(blob.len(), test_case.expected_len);
                }
                _ => panic!("exec_randomblob did not return a Blob variant"),
            }
        }
    }

    #[test]
    fn test_exec_round() {
        let input_val = OwnedValue::Float(123.456);
        let expected_val = OwnedValue::Float(123.0);
        assert_eq!(exec_round(&input_val, None), expected_val);

        let input_val = OwnedValue::Float(123.456);
        let precision_val = OwnedValue::Integer(2);
        let expected_val = OwnedValue::Float(123.46);
        assert_eq!(exec_round(&input_val, Some(&precision_val)), expected_val);

        let input_val = OwnedValue::Float(123.456);
        let precision_val = OwnedValue::build_text("1");
        let expected_val = OwnedValue::Float(123.5);
        assert_eq!(exec_round(&input_val, Some(&precision_val)), expected_val);

        let input_val = OwnedValue::build_text("123.456");
        let precision_val = OwnedValue::Integer(2);
        let expected_val = OwnedValue::Float(123.46);
        assert_eq!(exec_round(&input_val, Some(&precision_val)), expected_val);

        let input_val = OwnedValue::Integer(123);
        let precision_val = OwnedValue::Integer(1);
        let expected_val = OwnedValue::Float(123.0);
        assert_eq!(exec_round(&input_val, Some(&precision_val)), expected_val);

        let input_val = OwnedValue::Float(100.123);
        let expected_val = OwnedValue::Float(100.0);
        assert_eq!(exec_round(&input_val, None), expected_val);

        let input_val = OwnedValue::Float(100.123);
        let expected_val = OwnedValue::Null;
        assert_eq!(
            exec_round(&input_val, Some(&OwnedValue::Null)),
            expected_val
        );
    }

    #[test]
    fn test_exec_if() {
        let reg = OwnedValue::Integer(0);
        assert!(!exec_if(&reg, false, false));
        assert!(exec_if(&reg, false, true));

        let reg = OwnedValue::Integer(1);
        assert!(exec_if(&reg, false, false));
        assert!(!exec_if(&reg, false, true));

        let reg = OwnedValue::Null;
        assert!(!exec_if(&reg, false, false));
        assert!(!exec_if(&reg, false, true));

        let reg = OwnedValue::Null;
        assert!(exec_if(&reg, true, false));
        assert!(exec_if(&reg, true, true));

        let reg = OwnedValue::Null;
        assert!(!exec_if(&reg, false, false));
        assert!(!exec_if(&reg, false, true));
    }

    #[test]
    fn test_nullif() {
        assert_eq!(
            exec_nullif(&OwnedValue::Integer(1), &OwnedValue::Integer(1)),
            OwnedValue::Null
        );
        assert_eq!(
            exec_nullif(&OwnedValue::Float(1.1), &OwnedValue::Float(1.1)),
            OwnedValue::Null
        );
        assert_eq!(
            exec_nullif(
                &OwnedValue::build_text("limbo"),
                &OwnedValue::build_text("limbo")
            ),
            OwnedValue::Null
        );

        assert_eq!(
            exec_nullif(&OwnedValue::Integer(1), &OwnedValue::Integer(2)),
            OwnedValue::Integer(1)
        );
        assert_eq!(
            exec_nullif(&OwnedValue::Float(1.1), &OwnedValue::Float(1.2)),
            OwnedValue::Float(1.1)
        );
        assert_eq!(
            exec_nullif(
                &OwnedValue::build_text("limbo"),
                &OwnedValue::build_text("limb")
            ),
            OwnedValue::build_text("limbo")
        );
    }

    #[test]
    fn test_substring() {
        let str_value = OwnedValue::build_text("limbo");
        let start_value = OwnedValue::Integer(1);
        let length_value = OwnedValue::Integer(3);
        let expected_val = OwnedValue::build_text("lim");
        assert_eq!(
            exec_substring(&str_value, &start_value, Some(&length_value)),
            expected_val
        );

        let str_value = OwnedValue::build_text("limbo");
        let start_value = OwnedValue::Integer(1);
        let length_value = OwnedValue::Integer(10);
        let expected_val = OwnedValue::build_text("limbo");
        assert_eq!(
            exec_substring(&str_value, &start_value, Some(&length_value)),
            expected_val
        );

        let str_value = OwnedValue::build_text("limbo");
        let start_value = OwnedValue::Integer(10);
        let length_value = OwnedValue::Integer(3);
        let expected_val = OwnedValue::build_text("");
        assert_eq!(
            exec_substring(&str_value, &start_value, Some(&length_value)),
            expected_val
        );

        let str_value = OwnedValue::build_text("limbo");
        let start_value = OwnedValue::Integer(3);
        let length_value = OwnedValue::Null;
        let expected_val = OwnedValue::build_text("mbo");
        assert_eq!(
            exec_substring(&str_value, &start_value, Some(&length_value)),
            expected_val
        );

        let str_value = OwnedValue::build_text("limbo");
        let start_value = OwnedValue::Integer(10);
        let length_value = OwnedValue::Null;
        let expected_val = OwnedValue::build_text("");
        assert_eq!(
            exec_substring(&str_value, &start_value, Some(&length_value)),
            expected_val
        );
    }

    #[test]
    fn test_exec_instr() {
        let input = OwnedValue::build_text("limbo");
        let pattern = OwnedValue::build_text("im");
        let expected = OwnedValue::Integer(2);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("limbo");
        let pattern = OwnedValue::build_text("limbo");
        let expected = OwnedValue::Integer(1);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("limbo");
        let pattern = OwnedValue::build_text("o");
        let expected = OwnedValue::Integer(5);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("liiiiimbo");
        let pattern = OwnedValue::build_text("ii");
        let expected = OwnedValue::Integer(2);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("limbo");
        let pattern = OwnedValue::build_text("limboX");
        let expected = OwnedValue::Integer(0);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("limbo");
        let pattern = OwnedValue::build_text("");
        let expected = OwnedValue::Integer(1);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("");
        let pattern = OwnedValue::build_text("limbo");
        let expected = OwnedValue::Integer(0);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("");
        let pattern = OwnedValue::build_text("");
        let expected = OwnedValue::Integer(1);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Null;
        let pattern = OwnedValue::Null;
        let expected = OwnedValue::Null;
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("limbo");
        let pattern = OwnedValue::Null;
        let expected = OwnedValue::Null;
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Null;
        let pattern = OwnedValue::build_text("limbo");
        let expected = OwnedValue::Null;
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Integer(123);
        let pattern = OwnedValue::Integer(2);
        let expected = OwnedValue::Integer(2);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Integer(123);
        let pattern = OwnedValue::Integer(5);
        let expected = OwnedValue::Integer(0);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Float(12.34);
        let pattern = OwnedValue::Float(2.3);
        let expected = OwnedValue::Integer(2);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Float(12.34);
        let pattern = OwnedValue::Float(5.6);
        let expected = OwnedValue::Integer(0);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Float(12.34);
        let pattern = OwnedValue::build_text(".");
        let expected = OwnedValue::Integer(3);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Blob(Rc::new(vec![1, 2, 3, 4, 5]));
        let pattern = OwnedValue::Blob(Rc::new(vec![3, 4]));
        let expected = OwnedValue::Integer(3);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Blob(Rc::new(vec![1, 2, 3, 4, 5]));
        let pattern = OwnedValue::Blob(Rc::new(vec![3, 2]));
        let expected = OwnedValue::Integer(0);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::Blob(Rc::new(vec![0x61, 0x62, 0x63, 0x64, 0x65]));
        let pattern = OwnedValue::build_text("cd");
        let expected = OwnedValue::Integer(3);
        assert_eq!(exec_instr(&input, &pattern), expected);

        let input = OwnedValue::build_text("abcde");
        let pattern = OwnedValue::Blob(Rc::new(vec![0x63, 0x64]));
        let expected = OwnedValue::Integer(3);
        assert_eq!(exec_instr(&input, &pattern), expected);
    }

    #[test]
    fn test_exec_sign() {
        let input = OwnedValue::Integer(42);
        let expected = Some(OwnedValue::Integer(1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Integer(-42);
        let expected = Some(OwnedValue::Integer(-1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Integer(0);
        let expected = Some(OwnedValue::Integer(0));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Float(0.0);
        let expected = Some(OwnedValue::Integer(0));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Float(0.1);
        let expected = Some(OwnedValue::Integer(1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Float(42.0);
        let expected = Some(OwnedValue::Integer(1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Float(-42.0);
        let expected = Some(OwnedValue::Integer(-1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::build_text("abc");
        let expected = Some(OwnedValue::Null);
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::build_text("42");
        let expected = Some(OwnedValue::Integer(1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::build_text("-42");
        let expected = Some(OwnedValue::Integer(-1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::build_text("0");
        let expected = Some(OwnedValue::Integer(0));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Blob(Rc::new(b"abc".to_vec()));
        let expected = Some(OwnedValue::Null);
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Blob(Rc::new(b"42".to_vec()));
        let expected = Some(OwnedValue::Integer(1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Blob(Rc::new(b"-42".to_vec()));
        let expected = Some(OwnedValue::Integer(-1));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Blob(Rc::new(b"0".to_vec()));
        let expected = Some(OwnedValue::Integer(0));
        assert_eq!(exec_sign(&input), expected);

        let input = OwnedValue::Null;
        let expected = Some(OwnedValue::Null);
        assert_eq!(exec_sign(&input), expected);
    }

    #[test]
    fn test_exec_zeroblob() {
        let input = OwnedValue::Integer(0);
        let expected = OwnedValue::Blob(Rc::new(vec![]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::Null;
        let expected = OwnedValue::Blob(Rc::new(vec![]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::Integer(4);
        let expected = OwnedValue::Blob(Rc::new(vec![0; 4]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::Integer(-1);
        let expected = OwnedValue::Blob(Rc::new(vec![]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::build_text("5");
        let expected = OwnedValue::Blob(Rc::new(vec![0; 5]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::build_text("-5");
        let expected = OwnedValue::Blob(Rc::new(vec![]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::build_text("text");
        let expected = OwnedValue::Blob(Rc::new(vec![]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::Float(2.6);
        let expected = OwnedValue::Blob(Rc::new(vec![0; 2]));
        assert_eq!(exec_zeroblob(&input), expected);

        let input = OwnedValue::Blob(Rc::new(vec![1]));
        let expected = OwnedValue::Blob(Rc::new(vec![]));
        assert_eq!(exec_zeroblob(&input), expected);
    }

    #[test]
    fn test_execute_sqlite_version() {
        let version_integer = 3046001;
        let expected = "3.46.1";
        assert_eq!(execute_sqlite_version(version_integer), expected);
    }

    #[test]
    fn test_replace() {
        let input_str = OwnedValue::build_text("bob");
        let pattern_str = OwnedValue::build_text("b");
        let replace_str = OwnedValue::build_text("a");
        let expected_str = OwnedValue::build_text("aoa");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bob");
        let pattern_str = OwnedValue::build_text("b");
        let replace_str = OwnedValue::build_text("");
        let expected_str = OwnedValue::build_text("o");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bob");
        let pattern_str = OwnedValue::build_text("b");
        let replace_str = OwnedValue::build_text("abc");
        let expected_str = OwnedValue::build_text("abcoabc");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bob");
        let pattern_str = OwnedValue::build_text("a");
        let replace_str = OwnedValue::build_text("b");
        let expected_str = OwnedValue::build_text("bob");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bob");
        let pattern_str = OwnedValue::build_text("");
        let replace_str = OwnedValue::build_text("a");
        let expected_str = OwnedValue::build_text("bob");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bob");
        let pattern_str = OwnedValue::Null;
        let replace_str = OwnedValue::build_text("a");
        let expected_str = OwnedValue::Null;
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bo5");
        let pattern_str = OwnedValue::Integer(5);
        let replace_str = OwnedValue::build_text("a");
        let expected_str = OwnedValue::build_text("boa");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bo5.0");
        let pattern_str = OwnedValue::Float(5.0);
        let replace_str = OwnedValue::build_text("a");
        let expected_str = OwnedValue::build_text("boa");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bo5");
        let pattern_str = OwnedValue::Float(5.0);
        let replace_str = OwnedValue::build_text("a");
        let expected_str = OwnedValue::build_text("bo5");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        let input_str = OwnedValue::build_text("bo5.0");
        let pattern_str = OwnedValue::Float(5.0);
        let replace_str = OwnedValue::Float(6.0);
        let expected_str = OwnedValue::build_text("bo6.0");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );

        // todo: change this test to use (0.1 + 0.2) instead of 0.3 when decimals are implemented.
        let input_str = OwnedValue::build_text("tes3");
        let pattern_str = OwnedValue::Integer(3);
        let replace_str = OwnedValue::Float(0.3);
        let expected_str = OwnedValue::build_text("tes0.3");
        assert_eq!(
            exec_replace(&input_str, &pattern_str, &replace_str),
            expected_str
        );
    }

    #[test]
    fn test_bitfield() {
        let mut bitfield = Bitfield::<4>::new();
        for i in 0..256 {
            bitfield.set(i);
            assert!(bitfield.get(i));
            for j in 0..i {
                assert!(bitfield.get(j));
            }
            for j in i + 1..256 {
                assert!(!bitfield.get(j));
            }
        }
        for i in 0..256 {
            bitfield.unset(i);
            assert!(!bitfield.get(i));
        }
    }
}

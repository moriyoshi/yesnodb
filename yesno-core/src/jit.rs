//! Cranelift execution for fused bitmap expression DAGs.
//!
//! This optional module keeps Cranelift outside the default dependency graph.
//! The ordinary planned [`Expr`] stream remains the correctness oracle and
//! fallback on unsupported shapes and hosts.
//!
//! A supported expression is planned once, flattened to postfix instructions,
//! and cached by that shape rather than by its input sets. Explicit `DagJit`
//! derives candidate prefixes from payload-free lower-bound peeks over the
//! whole DAG, seeks lagging leaves, and loads only leaves whose branch can
//! contribute.
//! A later chunk returned after a loose peek stays buffered. At a candidate
//! prefix, bitmap leaves feed one compiled vector loop, which returns only
//! the DAG popcount. Sparse gaps use one shared zero chunk. Arrays, runs and
//! unaligned mmap bitmaps stay correct through core's audited kernels.
//! Generated code remains owned by its cache entry and is explicitly unmapped
//! when that entry or its cache is dropped. Compilation errors use the same
//! owner, so partially generated code is also reclaimed.
//!
//! # Host support
//!
//! Code generation is enabled on AArch64 and x86_64. Both are allow-listed
//! rather than probed, because a backend missing a rule for one of the emitted
//! operations panics inside lowering rather than returning anything there is to
//! catch. The reduction is the part that differs: `popcnt` over `i8x16`
//! followed by the canonical `iadd_pairwise` / `uwiden` pair is one `cnt` plus
//! `uaddlp` on AArch64, while on x86_64 it is whichever of **three** lowering
//! tiers the host's CPU features select -- `vpopcntb` under AVX512VL and
//! AVX512BITALG, Mula's `pshufb` nibble table under SSSE3, or a shift-and-mask
//! fallback on bare SSE2. The two widening steps have their own fused x64
//! rules for exactly this pattern ( `pmaddubsw`, then `pmaddwd` ), which is why
//! the pairwise form must not be rewritten into ordinary widening. Every other
//! host declines and the ordinary evaluator runs.
//!
//! The SSSE3 and SSE2 tiers are both exercised: `qemu-x86_64`'s default
//! `qemu64` model reports no SSSE3 and `-cpu Haswell` reports SSSE3 and AVX, so
//! running the tests under each covers both, and `-d in_asm` confirms the
//! executed code of the second contains `vpshufb`, `vpmaddubsw` and `vpmaddwd`.
//! **The `vpopcntb` tier is unexercised anywhere currently available**: QEMU's
//! TCG implements no AVX512, and the Intel i9-9880H that settles this
//! repository's x86 questions is Coffee Lake, which has no AVX512BITALG. It
//! needs an Ice Lake, Sapphire Rapids or Zen 4 host.
//!
//! **Automatic** admission through [`cardinality`] is enabled on both hosts.
//! It was gated to AArch64 when x86_64 code generation first landed, on the
//! argument that core's own bitmap kernels are AVX2 there ( 2.70x over scalar
//! `popcnt` for a 1 024-word popcount, recorded in `ops::bitmap` ) while
//! Cranelift IR caps vectors at 128 bits, so the generated loop would be
//! competing at half the width of the path it replaces. **Measured on an Intel
//! i9-9880H, that argument does not hold.** At the admission floor the six
//! benchmark shapes are 6.65x, 13.39x, 6.25x, 1.64x, 1.82x and 16.71x on x86_64
//! against 7.86x, 11.51x, 5.14x, 1.51x, 1.66x and 12.50x on AArch64 -- within
//! about 15% per shape, and a win on every one. The gain is fusion, and fusion
//! does not care about vector width.
//!
//! Automatic admission additionally requires at least four leaves, and every
//! leaf to expose the same contiguous span of at least `AUTO_MIN_CHUNKS`
//! bitmap chunks. Automatic execution retains the measured dense union walk;
//! explicit `DagJit` uses the seek-driven walk for sparse inputs. Arrays still
//! take the generic-prefix fallback, and an aligned two-leaf bitmap AND was
//! slower through the JIT than core's tuned binary cardinality path while a
//! four-leaf mixed DAG retained a measured
//! win. **Those two floors were measured on AArch64 and are applied on both
//! hosts**: they only ever narrow admission, and the shapes they exclude are
//! precisely the ones the automatic benchmark does not cover.
//!
//! # The explicit surface is broader, and part of it is slower
//!
//! Explicit [`DagJit`] accepts shapes automatic admission declines, and **on
//! some of them it loses to core, by a lot**. Measured 2026-09-22 over three
//! complete runs per host against core with normal planning and identical
//! resident leaves, one wide 256-chunk leaf against a narrow leaf of `n`
//! chunks spread through that span, as core/JIT ratios ( under 1.00x means
//! core is faster ):
//!
//! ```text
//!                       n=1     n=4    n=16    n=64   n=256
//!   2-leaf AND  x86    0.45x   0.44x   0.44x   0.51x   0.49x
//!               arm    0.57x   0.65x   0.69x   0.72x   0.67x
//!   AND under   x86    2.15x   4.26x   5.93x   6.52x   6.83x
//!     an OR     arm    2.88x   3.53x   4.63x   6.40x   7.12x
//!   4-leaf with x86    0.04x   0.11x   0.46x   1.81x   6.48x
//!     AND-NOT   arm    0.04x   0.09x   0.34x   1.70x   6.33x
//! ```
//!
//! Every cell agrees in sign across the two architectures. Two shapes to know:
//!
//! A plain two-leaf selective AND loses everywhere. Its cost *does* track
//! selectivity, so the seek-driven walk is working; it simply does not beat
//! core's tuned binary cardinality path, and it loses by more on x86 because
//! that path is AVX2 there. This is the measurement behind the four-leaf floor.
//!
//! A four-leaf DAG whose second branch is `and_not` over a wide leaf cannot
//! skip any prefix -- `and_not` needs the wide side everywhere, so this is
//! correct rather than a defect -- and its cost is flat in `n` while core's
//! scales with it. That crosses 1.00x between `n=16` and `n=64`, roughly a
//! sixth of the span, and below that the JIT is up to **25x slower**. Reach
//! for explicit `DagJit` on a selective shape only with a measurement of that
//! shape.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::ops;
use crate::stream::{BoxedStream, ChunkSource, SetStream};
use crate::unstable_arrow::bitmap_words;
use crate::{Container, Expr, OrdSet, Prefix48, BITMAP_WORDS};
use cranelift_codegen::ir::{types, AbiParam, Endianness, InstBuilder, MemFlagsData, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};

const MAX_LEAVES: usize = 32;
const MAX_INSTRUCTIONS: usize = MAX_LEAVES * 2 - 1;
const MAX_CACHED_SHAPES: usize = 64;
const AUTO_MIN_CHUNKS: u64 = 256;
const UNROLL: usize = 2;
static ZERO_WORDS: [u64; BITMAP_WORDS] = [0; BITMAP_WORDS];

/// Hosts whose Cranelift backend is known to lower every operation this
/// generator emits. An allow-list and not a fallible probe, for the reason the
/// module header gives.
const fn generator_supported() -> bool {
    cfg!(any(target_arch = "aarch64", target_arch = "x86_64"))
}

/// Hosts where automatic admission has been measured against the core
/// evaluator `cardinality` would otherwise call.
///
/// Identical to [`generator_supported`] today, and kept separate anyway. The
/// two answer different questions -- "does this backend lower our IR" and "has
/// the ratio been measured here" -- and allow-listing a third architecture
/// must not silently switch automatic admission on for it. Widen this one only
/// with a benchmark run behind it.
const fn auto_admission_supported() -> bool {
    cfg!(any(target_arch = "aarch64", target_arch = "x86_64"))
}

type Kernel = unsafe extern "C" fn(*const *const u64) -> u64;
#[derive(Clone, Copy)]
enum Traversal {
    Seek,
    DenseUnion,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Instruction {
    Leaf(u8),
    Zero,
    And,
    Or,
    Xor,
    AndNot,
}

struct Program {
    code: Vec<Instruction>,
    leaves: Vec<Leaf>,
}

enum Leaf {
    Set(Arc<OrdSet>),
    Source(Arc<dyn ChunkSource>),
}

impl Program {
    fn from_expr(expr: &Expr) -> Option<Self> {
        let mut program = Self {
            code: Vec::new(),
            leaves: Vec::new(),
        };
        program.lower(expr)?;
        (program.leaves.len() >= 2 && program.code.len() <= MAX_INSTRUCTIONS).then_some(program)
    }

    fn lower(&mut self, expr: &Expr) -> Option<()> {
        if self.code.len() >= MAX_INSTRUCTIONS {
            return None;
        }
        match expr {
            Expr::Set(set) => {
                if self.leaves.len() >= MAX_LEAVES {
                    return None;
                }
                let index = self.leaves.len() as u8;
                self.leaves.push(Leaf::Set(set.clone()));
                self.code.push(Instruction::Leaf(index));
            }
            Expr::Source(source) => {
                if self.leaves.len() >= MAX_LEAVES {
                    return None;
                }
                let index = self.leaves.len() as u8;
                self.leaves.push(Leaf::Source(source.clone()));
                self.code.push(Instruction::Leaf(index));
            }
            Expr::Empty => self.code.push(Instruction::Zero),
            Expr::And(a, b) => self.lower_binary(a, b, Instruction::And)?,
            Expr::Or(a, b) => self.lower_binary(a, b, Instruction::Or)?,
            Expr::Xor(a, b) => self.lower_binary(a, b, Instruction::Xor)?,
            Expr::AndNot(a, b) => self.lower_binary(a, b, Instruction::AndNot)?,
            Expr::Range(_, _) | Expr::Not(_, _, _) => return None,
        }
        Some(())
    }

    fn lower_binary(&mut self, a: &Expr, b: &Expr, op: Instruction) -> Option<()> {
        self.lower(a)?;
        self.lower(b)?;
        if self.code.len() >= MAX_INSTRUCTIONS {
            return None;
        }
        self.code.push(op);
        Some(())
    }
}

struct Compiled {
    _module: OwnedModule,
    function: Kernel,
}

/// Owns executable mappings from module construction through every failure
/// path, and for the full lifetime of a successfully compiled kernel.
struct OwnedModule(Option<JITModule>);

impl Deref for OwnedModule {
    type Target = JITModule;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("JIT module is owned until drop")
    }
}

impl DerefMut for OwnedModule {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("JIT module is owned until drop")
    }
}

impl Drop for OwnedModule {
    fn drop(&mut self) {
        if let Some(module) = self.0.take() {
            // SAFETY: generated pointers never escape Compiled; calls require
            // mutable access to the owning DagJit, so no generated function
            // can still be executing when this owner is dropped. On compile
            // errors, no generated pointer has been exposed at all.
            unsafe { module.free_memory() };
        }
    }
}

/// A shape cache and compiler for fused bitmap cardinality expressions.
///
/// `DagJit` is intentionally not global and makes no `Send` or `Sync` promise.
/// Use one per worker, or call [`cardinality`] for a thread-local cache.
#[derive(Default)]
pub struct DagJit {
    compiled: HashMap<Vec<Instruction>, Compiled>,
    failed: HashSet<Vec<Instruction>>,
}

impl std::fmt::Debug for DagJit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DagJit")
            .field("compiled_shapes", &self.compiled.len())
            .field("failed_shapes", &self.failed.len())
            .finish()
    }
}

impl DagJit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count `expression` with a fused kernel when its planned leaves are
    /// resident sets or lazy sources. Returns `None` when the ordinary core
    /// evaluator should be used instead.
    pub fn try_cardinality(&mut self, expression: &Expr) -> Option<crate::Result<u64>> {
        self.try_cardinality_with(expression, Traversal::Seek)
    }

    // The automatic gate admits only dense aligned leaves. Keep its measured
    // union walk, including on x86_64, while explicit calls use selective seek.
    fn try_cardinality_auto(&mut self, expression: &Expr) -> Option<crate::Result<u64>> {
        self.try_cardinality_with(expression, Traversal::DenseUnion)
    }

    fn try_cardinality_with(
        &mut self,
        expression: &Expr,
        traversal: Traversal,
    ) -> Option<crate::Result<u64>> {
        let planned = expression.plan();
        let program = Program::from_expr(&planned)?;
        let key = program.code.clone();
        if self.failed.contains(&key) {
            return None;
        }
        if !self.compiled.contains_key(&key) {
            // Executable mappings are kept until this cache is dropped. An
            // unbounded shape cache would let unique wire queries grow them
            // forever; once full, the generic evaluator remains exact.
            if self.compiled.len() + self.failed.len() >= MAX_CACHED_SHAPES {
                return None;
            }
            match compile(&program.code, program.leaves.len()) {
                Ok(compiled) => {
                    self.compiled.insert(key.clone(), compiled);
                }
                Err(()) => {
                    self.failed.insert(key);
                    return None;
                }
            }
        }
        let kernel = self.compiled.get(&key)?.function;
        Some(match traversal {
            Traversal::Seek => execute_seek(kernel, &program),
            Traversal::DenseUnion => execute_dense_union(kernel, &program),
        })
    }

    pub fn compiled_shapes(&self) -> usize {
        self.compiled.len()
    }
}

thread_local! {
    static LOCAL_JIT: RefCell<DagJit> = RefCell::new(DagJit::new());
}

/// Count with the current thread's JIT cache, falling back transparently to
/// the ordinary core evaluator for unsupported expressions or hosts.
pub fn cardinality(expression: &Expr) -> crate::Result<u64> {
    if !auto_admission_supported() || !worth_jitting(expression) {
        return expression.cardinality();
    }
    if let Some(count) = LOCAL_JIT.with(|jit| jit.borrow_mut().try_cardinality_auto(expression)) {
        count
    } else {
        expression.cardinality()
    }
}

/// Automatic compilation is reserved for aligned, contiguous bitmap DAGs.
///
/// **The throughput figures this comment used to cite are withdrawn.** They
/// read "1 chunk 0.58x, 16 chunks 1.06x, 64 chunks 1.04x, 256 chunks 1.12x"
/// and do not reproduce, for two reasons that compound: they timed *prepared*
/// core evaluation, where this function's caller builds an `Expr` per request
/// and counts it once, and they ran on a corpus whose leaves did not overlap,
/// which collapsed every AND-heavy shape to effectively one leaf. Re-measured
/// with `benches/dag.rs`, three complete runs per host, the six shapes at 256
/// chunks are 6.65x / 13.39x / 6.25x / 1.64x / 1.82x / 16.71x on an Intel
/// i9-9880H and 7.86x / 11.51x / 5.14x / 1.51x / 1.66x / 12.50x on AArch64.
/// Compile costs 0.6-4.6 ms per shape on that x86 host and 0.1-1.4 ms on
/// AArch64, which the 256-chunk saving repays inside a single call.
///
/// `AUTO_MIN_CHUNKS` is kept at 256 regardless. One chunk now measures in the
/// JIT's favour as well ( 1.31x-7.90x ), so the constant no longer rests on a
/// throughput cliff -- but the break-even there is 20 to 200 repetitions of the
/// same shape, which makes it a bet about shape reuse, and nothing has measured
/// shape reuse. Lowering it needs that measurement, not this one.
///
/// The structural floors below are a separate question and were measured on
/// AArch64. The automatic dense-union walk would still lose selective ANDs
/// and array fallbacks; explicit `DagJit` uses a separate seek-driven walk.
/// Unknown source encodings and non-dense spans therefore decline automatic
/// admission. A plain aligned binary AND loses to core's tuned binary path,
/// so automatic admission requires at least four leaves. Explicit `DagJit`
/// remains available for all of those shapes.
fn worth_jitting(expression: &Expr) -> bool {
    // Reject simple native binary paths before scanning container tags or
    // asking sources for metadata. Count only as far as the admission floor.
    fn leaf_count_bounded(expr: &Expr) -> usize {
        match expr {
            Expr::Set(_) | Expr::Source(_) => 1,
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) | Expr::AndNot(a, b) => {
                (leaf_count_bounded(a) + leaf_count_bounded(b)).min(4)
            }
            Expr::Empty | Expr::Range(_, _) | Expr::Not(_, _, _) => 0,
        }
    }

    if leaf_count_bounded(expression) < 4 {
        return false;
    }

    fn dense_bitmap_leaves(expr: &Expr, common_span: &mut Option<(Prefix48, Prefix48)>) -> bool {
        let (chunks, (lo, hi)) = match expr {
            Expr::Set(set) => {
                let chunks = set.chunk_count() as u64;
                if chunks < AUTO_MIN_CHUNKS
                    || !set
                        .chunks()
                        .all(|(_, container)| matches!(container, Container::Bitmap(_)))
                {
                    return false;
                }
                let lo = set.chunk_at(0).expect("nonempty set").0;
                let hi = set.chunk_at(set.chunk_count() - 1).expect("nonempty set").0;
                (chunks, (lo, hi))
            }
            Expr::Source(source) => {
                let Some(chunks) = source.chunk_count() else {
                    return false;
                };
                let Some(span) = source.prefix_span() else {
                    return false;
                };
                if chunks < AUTO_MIN_CHUNKS || source.all_bitmap_chunks() != Some(true) {
                    return false;
                }
                (chunks, span)
            }
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) | Expr::AndNot(a, b) => {
                return dense_bitmap_leaves(a, common_span) && dense_bitmap_leaves(b, common_span);
            }
            Expr::Empty | Expr::Range(_, _) | Expr::Not(_, _, _) => return false,
        };

        // Equal dense spans guarantee that every leaf has a bitmap at every
        // visited prefix. Sparse or selective inputs belong to core's
        // seek-driven walk; the automatic dense executor drains their union.
        let Some(width) = hi
            .checked_sub(lo)
            .and_then(|distance| distance.checked_add(1))
        else {
            return false;
        };
        if hi >= 1u64 << 48 || width != chunks {
            return false;
        }
        match common_span {
            Some(span) if *span != (lo, hi) => return false,
            Some(_) => {}
            None => *common_span = Some((lo, hi)),
        }
        true
    }

    let mut common_span = None;
    dense_bitmap_leaves(expression, &mut common_span)
}

// A buffered payload remains logically ahead of its stream until consumed.
struct PrefixCursor {
    stream: BoxedStream,
    pending: Option<(Prefix48, Container)>,
    floor: Prefix48,
    exhausted: bool,
}

impl PrefixCursor {
    fn new(stream: BoxedStream) -> Self {
        Self {
            stream,
            pending: None,
            floor: 0,
            exhausted: false,
        }
    }

    fn peek(&mut self) -> crate::Result<Option<Prefix48>> {
        if let Some((prefix, _)) = &self.pending {
            return Ok(Some(*prefix));
        }
        if self.exhausted {
            return Ok(None);
        }
        // A stream may report only a lower bound. Seeking or consuming gives
        // us a stronger floor, which is still no greater than its next chunk.
        Ok(self.stream.peek_prefix()?.map(|p| p.max(self.floor)))
    }

    fn seek(&mut self, prefix: Prefix48) -> crate::Result<()> {
        self.floor = self.floor.max(prefix);
        if self
            .pending
            .as_ref()
            .is_some_and(|(candidate, _)| *candidate >= prefix)
        {
            return Ok(());
        }
        self.pending = None;
        if !self.exhausted {
            self.stream.seek(prefix)?;
        }
        Ok(())
    }

    fn load(&mut self) -> crate::Result<()> {
        if self.pending.is_none() && !self.exhausted {
            self.pending = self.stream.next_chunk()?;
            self.exhausted = self.pending.is_none();
        }
        Ok(())
    }

    fn advance(&mut self, prefix: Prefix48) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(candidate, _)| *candidate == prefix)
        {
            self.pending = None;
            self.floor = self.floor.max(prefix + 1);
        }
    }
}

#[derive(Clone, Copy)]
struct PrefixState {
    bound: Option<Prefix48>,
    leaves: u32,
}

// Evaluate a lower bound for the whole DAG and mark only leaves that can
// affect that candidate. AND raises the bound, OR/XOR choose the earlier
// branch, and ANDNOT follows its left branch while checking a coincident
// right branch for removal.
fn prefix_candidate(
    code: &[Instruction],
    peeks: &[Option<Prefix48>],
    stack: &mut Vec<PrefixState>,
) -> Option<(Prefix48, u32)> {
    stack.clear();
    for instruction in code {
        match *instruction {
            Instruction::Leaf(index) => stack.push(PrefixState {
                bound: peeks[index as usize],
                leaves: 1u32 << index,
            }),
            Instruction::Zero => stack.push(PrefixState {
                bound: None,
                leaves: 0,
            }),
            op => {
                let right = stack.pop().expect("validated postfix right operand");
                let left = stack.pop().expect("validated postfix left operand");
                let state = match op {
                    Instruction::And => {
                        let bound = left.bound.zip(right.bound).map(|(a, b)| a.max(b));
                        let leaves = (if left.bound == bound { left.leaves } else { 0 })
                            | (if right.bound == bound {
                                right.leaves
                            } else {
                                0
                            });
                        PrefixState { bound, leaves }
                    }
                    Instruction::Or | Instruction::Xor => {
                        let bound = left.bound.into_iter().chain(right.bound).min();
                        let leaves = (if left.bound == bound { left.leaves } else { 0 })
                            | (if right.bound == bound {
                                right.leaves
                            } else {
                                0
                            });
                        PrefixState { bound, leaves }
                    }
                    Instruction::AndNot => PrefixState {
                        bound: left.bound,
                        leaves: left.leaves
                            | if right.bound == left.bound {
                                right.leaves
                            } else {
                                0
                            },
                    },
                    Instruction::Leaf(_) | Instruction::Zero => unreachable!(),
                };
                stack.push(state);
            }
        }
    }
    let root = stack.pop().expect("a program has a root");
    debug_assert!(stack.is_empty());
    root.bound.map(|prefix| (prefix, root.leaves))
}

fn execute_seek(kernel: Kernel, program: &Program) -> crate::Result<u64> {
    let mut cursors: Vec<PrefixCursor> = program
        .leaves
        .iter()
        .map(|leaf| match leaf {
            Leaf::Set(set) => Box::new(SetStream::new(set.clone())) as BoxedStream,
            Leaf::Source(source) => source.open(),
        })
        .map(PrefixCursor::new)
        .collect();
    let mut peeks = vec![None; cursors.len()];
    let mut stack = Vec::with_capacity(program.code.len());
    let mut pointers = vec![ZERO_WORDS.as_ptr(); cursors.len()];
    let mut total = 0u64;

    loop {
        for (peek, cursor) in peeks.iter_mut().zip(&mut cursors) {
            *peek = cursor.peek()?;
        }
        let Some((prefix, active)) = prefix_candidate(&program.code, &peeks, &mut stack) else {
            break;
        };

        // No output precedes this bound. Advance lagging leaves without
        // loading payloads, then recompute: a lower-bound peek may move.
        let mut sought = false;
        for (cursor, peek) in cursors.iter_mut().zip(&peeks) {
            if peek.is_some_and(|p| p < prefix) {
                cursor.seek(prefix)?;
                sought = true;
            }
        }
        if sought {
            continue;
        }

        for (index, cursor) in cursors.iter_mut().enumerate() {
            if active & (1u32 << index) != 0 {
                debug_assert_eq!(peeks[index], Some(prefix));
                cursor.load()?;
            }
        }
        {
            let inputs: Vec<Option<&Container>> = cursors
                .iter()
                .enumerate()
                .map(|(index, cursor)| {
                    if active & (1u32 << index) == 0 {
                        return None;
                    }
                    match &cursor.pending {
                        Some((candidate, container)) if *candidate == prefix => Some(container),
                        _ => None,
                    }
                })
                .collect();

            total += count_prefix(kernel, &program.code, &inputs, &mut pointers);
        }
        for (index, cursor) in cursors.iter_mut().enumerate() {
            if active & (1u32 << index) != 0 {
                cursor.advance(prefix);
            }
        }
    }
    Ok(total)
}

fn execute_dense_union(kernel: Kernel, program: &Program) -> crate::Result<u64> {
    let mut streams: Vec<BoxedStream> = program
        .leaves
        .iter()
        .map(|leaf| match leaf {
            Leaf::Set(set) => Box::new(SetStream::new(set.clone())) as BoxedStream,
            Leaf::Source(source) => source.open(),
        })
        .collect();
    let mut current: Vec<Option<(Prefix48, Container)>> = streams
        .iter_mut()
        .map(|stream| stream.next_chunk())
        .collect::<crate::Result<_>>()?;
    let mut pointers = vec![ZERO_WORDS.as_ptr(); program.leaves.len()];
    let mut total = 0u64;

    loop {
        let prefix = current
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(p, _)| *p))
            .min();
        let Some(prefix) = prefix else {
            break;
        };

        {
            let inputs: Vec<Option<&Container>> = current
                .iter()
                .map(|entry| match entry {
                    Some((candidate, container)) if *candidate == prefix => Some(container),
                    _ => None,
                })
                .collect();
            total += count_prefix(kernel, &program.code, &inputs, &mut pointers);
        }
        for (stream, entry) in streams.iter_mut().zip(&mut current) {
            if entry
                .as_ref()
                .is_some_and(|(candidate, _)| *candidate == prefix)
            {
                *entry = stream.next_chunk()?;
            }
        }
    }
    Ok(total)
}

#[inline]
fn count_prefix(
    kernel: Kernel,
    code: &[Instruction],
    inputs: &[Option<&Container>],
    pointers: &mut [*const u64],
) -> u64 {
    debug_assert_eq!(inputs.len(), pointers.len());
    let mut lendable = true;
    for (slot, input) in pointers.iter_mut().zip(inputs) {
        *slot = match input {
            None => ZERO_WORDS.as_ptr(),
            Some(container) => match bitmap_words(container) {
                Some(words) if words.len() == BITMAP_WORDS => words.as_ptr(),
                _ => {
                    lendable = false;
                    ZERO_WORDS.as_ptr()
                }
            },
        };
    }

    if lendable {
        // SAFETY: There is one pointer per compiled leaf, each pointing to
        // BITMAP_WORDS live u64s borrowed from `inputs` or the static zero
        // chunk. Both executors call synchronously while their owning DagJit
        // still holds the executable module.
        unsafe { kernel(pointers.as_ptr()) }
    } else {
        u64::from(generic_prefix(code, inputs))
    }
}
fn generic_prefix(code: &[Instruction], inputs: &[Option<&Container>]) -> u32 {
    let mut stack: Vec<Option<Container>> = Vec::with_capacity(code.len());
    for instruction in code {
        match *instruction {
            Instruction::Leaf(index) => stack.push(inputs[index as usize].cloned()),
            Instruction::Zero => stack.push(None),
            op => {
                let right = stack.pop().expect("validated postfix right operand");
                let left = stack.pop().expect("validated postfix left operand");
                let value = match (op, left, right) {
                    (Instruction::And, Some(a), Some(b)) => ops::and(&a, &b),
                    (Instruction::And, _, _) => None,
                    (Instruction::Or | Instruction::Xor, None, value)
                    | (Instruction::Or | Instruction::Xor, value, None) => value,
                    (Instruction::Or, Some(a), Some(b)) => ops::or(&a, &b),
                    (Instruction::Xor, Some(a), Some(b)) => ops::xor(&a, &b),
                    (Instruction::AndNot, Some(a), Some(b)) => ops::and_not(&a, &b),
                    (Instruction::AndNot, value, None) => value,
                    (Instruction::AndNot, None, Some(_)) => None,
                    (Instruction::Leaf(_) | Instruction::Zero, _, _) => unreachable!(),
                };
                stack.push(value);
            }
        }
    }
    stack
        .pop()
        .expect("a program has a root")
        .map_or(0, |container| container.len())
}

fn compile(code: &[Instruction], leaf_count: usize) -> Result<Compiled, ()> {
    if !generator_supported() {
        return Err(());
    }

    let mut flags = settings::builder();
    flags
        .set("use_colocated_libcalls", "false")
        .map_err(|_| ())?;
    flags.set("is_pic", "false").map_err(|_| ())?;
    let isa = cranelift_native::builder()
        .map_err(|_| ())?
        .finish(settings::Flags::new(flags))
        .map_err(|_| ())?;
    let mut module = OwnedModule(Some(JITModule::new(JITBuilder::with_isa(
        isa,
        cranelift_module::default_libcall_names(),
    ))));
    let pointer_type = module.target_config().pointer_type();
    let frontend_config = module.target_config();
    let mut context = module.make_context();
    context
        .func
        .signature
        .params
        .push(AbiParam::new(pointer_type));
    context
        .func
        .signature
        .returns
        .push(AbiParam::new(types::I64));
    let id = module
        .declare_function("bitmap_dag", Linkage::Local, &context.func.signature)
        .map_err(|_| ())?;
    let mut builder_context = FunctionBuilderContext::new();

    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut builder_context);
        let entry = builder.create_block();
        let header = builder.create_block();
        let body = builder.create_block();
        let done = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.append_block_param(header, pointer_type);
        for _ in 0..UNROLL {
            builder.append_block_param(header, types::I32X4);
            builder.append_block_param(done, types::I32X4);
        }

        builder.switch_to_block(entry);
        let table = builder.block_params(entry)[0];
        let pointer_bytes = i32::from(module.target_config().pointer_bytes());
        let bases: Vec<Value> = (0..leaf_count)
            .map(|index| {
                builder.ins().load(
                    pointer_type,
                    MemFlagsData::new(),
                    table,
                    index as i32 * pointer_bytes,
                )
            })
            .collect();
        let zero_index = builder.ins().iconst(pointer_type, 0);
        let zero32 = builder.ins().iconst(types::I32, 0);
        let zero_accumulator = builder.ins().splat(types::I32X4, zero32);
        builder.ins().jump(
            header,
            &[
                zero_index.into(),
                zero_accumulator.into(),
                zero_accumulator.into(),
            ],
        );
        builder.seal_block(entry);

        builder.switch_to_block(header);
        let index = builder.block_params(header)[0];
        let counts = builder.block_params(header)[1..].to_vec();
        let limit = builder
            .ins()
            .iconst(pointer_type, (BITMAP_WORDS / (2 * UNROLL)) as i64);
        let inside = builder.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::UnsignedLessThan,
            index,
            limit,
        );
        builder.ins().brif(
            inside,
            body,
            &[],
            done,
            &[counts[0].into(), counts[1].into()],
        );

        builder.switch_to_block(body);
        let byte_offset = builder.ins().ishl_imm_u(index, 5);
        let addresses: Vec<Value> = bases
            .iter()
            .map(|base| builder.ins().iadd(*base, byte_offset))
            .collect();
        let mut lane_counts = Vec::with_capacity(UNROLL);
        for lane in 0..UNROLL {
            let leaves: Vec<Value> = addresses
                .iter()
                .map(|address| {
                    builder.ins().load(
                        types::I64X2,
                        MemFlagsData::new(),
                        *address,
                        (lane * 16) as i32,
                    )
                })
                .collect();
            let zero64 = builder.ins().iconst(types::I64, 0);
            let zero = builder.ins().splat(types::I64X2, zero64);
            let mut stack: Vec<Value> = Vec::with_capacity(code.len());
            for instruction in code {
                match *instruction {
                    Instruction::Leaf(index) => stack.push(leaves[index as usize]),
                    Instruction::Zero => stack.push(zero),
                    op => {
                        let right = stack.pop().expect("validated postfix right operand");
                        let left = stack.pop().expect("validated postfix left operand");
                        let value = match op {
                            Instruction::And => builder.ins().band(left, right),
                            Instruction::Or => builder.ins().bor(left, right),
                            Instruction::Xor => builder.ins().bxor(left, right),
                            Instruction::AndNot => {
                                let not_right = builder.ins().bnot(right);
                                builder.ins().band(left, not_right)
                            }
                            Instruction::Leaf(_) | Instruction::Zero => unreachable!(),
                        };
                        stack.push(value);
                    }
                }
            }
            let value = stack.pop().expect("a program has a root");
            let bytes = builder.ins().bitcast(
                types::I8X16,
                MemFlagsData::new().with_endianness(Endianness::Little),
                value,
            );
            let byte_counts = builder.ins().popcnt(bytes);
            let low16 = builder.ins().uwiden_low(byte_counts);
            let high16 = builder.ins().uwiden_high(byte_counts);
            let sums16 = builder.ins().iadd_pairwise(low16, high16);
            let low32 = builder.ins().uwiden_low(sums16);
            let high32 = builder.ins().uwiden_high(sums16);
            lane_counts.push(builder.ins().iadd_pairwise(low32, high32));
        }
        let next0 = builder.ins().iadd(counts[0], lane_counts[0]);
        let next1 = builder.ins().iadd(counts[1], lane_counts[1]);
        let next_index = builder.ins().iadd_imm_u(index, 1);
        builder
            .ins()
            .jump(header, &[next_index.into(), next0.into(), next1.into()]);
        builder.seal_block(body);
        builder.seal_block(header);

        builder.switch_to_block(done);
        let accumulators = builder.block_params(done).to_vec();
        let accumulator = builder.ins().iadd(accumulators[0], accumulators[1]);
        let paired = builder.ins().iadd_pairwise(accumulator, accumulator);
        let total = builder.ins().iadd_pairwise(paired, paired);
        let total = builder.ins().extractlane(total, 0);
        let total = builder.ins().uextend(types::I64, total);
        builder.ins().return_(&[total]);
        builder.seal_block(done);
        builder.finalize(frontend_config);
    }

    module.define_function(id, &mut context).map_err(|_| ())?;
    module.clear_context(&mut context);
    module.finalize_definitions().map_err(|_| ())?;
    let address = module.get_finalized_function(id);
    // SAFETY: Cranelift emitted `bitmap_dag` with exactly `Kernel`'s host C ABI
    // signature. `Compiled` retains the module and therefore its executable
    // allocation for every call through this pointer.
    let function = unsafe { std::mem::transmute::<*const u8, Kernel>(address) };
    Ok(Compiled {
        _module: module,
        function,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::ContainerKind;
    use proptest::prelude::*;

    fn bitmap_set(seed: u64, prefixes: &[u64]) -> Arc<OrdSet> {
        let mut values = Vec::new();
        for &prefix in prefixes {
            for low in 0..5000u64 {
                let mixed = low
                    .wrapping_mul(0x9e37_79b9)
                    .wrapping_add(seed.wrapping_mul(0x85eb_ca6b))
                    & 0xffff;
                values.push((prefix << 16) | mixed);
            }
        }
        Arc::new(OrdSet::from_iter_unsorted(values))
    }

    fn expression(sets: &[Arc<OrdSet>; 4]) -> Expr {
        Expr::set(sets[0].clone())
            .and(Expr::set(sets[1].clone()))
            .or(Expr::set(sets[2].clone()).and_not(Expr::set(sets[3].clone())))
            .xor(Expr::set(sets[0].clone()).and(Expr::set(sets[3].clone())))
    }

    #[derive(Debug)]
    struct ResidentSource(Arc<OrdSet>);

    impl ChunkSource for ResidentSource {
        fn open(&self) -> BoxedStream {
            Box::new(SetStream::new(self.0.clone()))
        }
    }

    #[derive(Debug)]
    struct FailingSource;

    impl ChunkSource for FailingSource {
        fn open(&self) -> BoxedStream {
            Box::new(crate::stream::ErrStream::new(crate::CodecError::Invariant(
                "source read failed",
            )))
        }
    }

    #[derive(Debug)]
    struct KnownSizeSource(Arc<OrdSet>);

    impl ChunkSource for KnownSizeSource {
        fn open(&self) -> BoxedStream {
            Box::new(SetStream::new(self.0.clone()))
        }

        fn chunk_count(&self) -> Option<u64> {
            Some(self.0.chunk_count() as u64)
        }

        fn prefix_span(&self) -> Option<(Prefix48, Prefix48)> {
            let lo = self.0.chunk_at(0)?.0;
            let hi = self.0.chunk_at(self.0.chunk_count() - 1)?.0;
            Some((lo, hi))
        }
    }

    #[derive(Debug)]
    struct MetadataTrapSource(Arc<OrdSet>);

    impl ChunkSource for MetadataTrapSource {
        fn open(&self) -> BoxedStream {
            Box::new(SetStream::new(self.0.clone()))
        }

        fn chunk_count(&self) -> Option<u64> {
            panic!("binary admission must decline before querying source metadata");
        }
    }

    #[test]
    fn automatic_admission_requires_aligned_contiguous_bitmap_leaves() {
        let dense_prefixes: Vec<_> = (0..AUTO_MIN_CHUNKS).collect();
        let first = bitmap_set(1, &dense_prefixes);
        let second = bitmap_set(2, &dense_prefixes);
        assert!(!worth_jitting(
            &Expr::set(first.clone()).and(Expr::set(second.clone()))
        ));
        let trap = Expr::Source(Arc::new(MetadataTrapSource(first.clone())));
        assert!(!worth_jitting(&trap.and(Expr::set(second.clone()))));
        let third = bitmap_set(3, &dense_prefixes);
        let fourth = bitmap_set(4, &dense_prefixes);
        let mixed = Expr::set(first.clone())
            .and(Expr::set(second.clone()))
            .or(Expr::set(third.clone()).and_not(Expr::set(fourth.clone())));
        assert!(worth_jitting(&mixed));
        // The public automatic entry must execute the admitted dense shape
        // with the same answer as the scalar core oracle.
        assert_eq!(cardinality(&mixed).unwrap(), mixed.cardinality().unwrap());

        let single = bitmap_set(1, &[128]);
        assert!(!worth_jitting(
            &Expr::set(first.clone()).and(Expr::set(single))
        ));
        let sparse_prefixes: Vec<_> = (0..AUTO_MIN_CHUNKS).map(|p| p * 2).collect();
        let sparse = bitmap_set(1, &sparse_prefixes);
        assert!(!worth_jitting(
            &Expr::set(sparse.clone()).and(Expr::set(sparse))
        ));

        let arrays = Arc::new(OrdSet::from_iter_unsorted(
            (0..AUTO_MIN_CHUNKS).map(|p| p << 16),
        ));
        assert!(!worth_jitting(
            &Expr::set(arrays.clone()).and(Expr::set(arrays))
        ));

        let unknown = Expr::Source(Arc::new(KnownSizeSource(first)));
        assert!(!worth_jitting(
            &unknown
                .and(Expr::set(second))
                .or(Expr::set(third).and_not(Expr::set(fourth)))
        ));
    }

    #[test]
    fn lazy_sources_are_fused_one_prefix_at_a_time() {
        let a = bitmap_set(5, &[0, 2, 3]);
        let b = bitmap_set(8, &[1, 2, 3]);
        let c = bitmap_set(13, &[2, 3, 4]);
        let expr = Expr::Source(Arc::new(ResidentSource(a)))
            .and(Expr::Source(Arc::new(ResidentSource(b))))
            .or(Expr::set(c));
        let expected = expr.cardinality().unwrap();
        let mut jit = DagJit::new();
        if generator_supported() {
            assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
        } else {
            assert!(jit.try_cardinality(&expr).is_none());
        }
        assert_eq!(cardinality(&expr).unwrap(), expected);
    }

    #[test]
    fn lazy_source_errors_propagate_instead_of_becoming_empty_chunks() {
        let expr = Expr::Source(Arc::new(FailingSource)).or(Expr::set(bitmap_set(1, &[0])));
        let mut jit = DagJit::new();
        if generator_supported() {
            assert!(jit.try_cardinality(&expr).unwrap().is_err());
        } else {
            assert!(jit.try_cardinality(&expr).is_none());
        }
        assert!(cardinality(&expr).is_err());
    }
    #[test]
    fn fused_dag_matches_core_across_missing_and_mixed_prefixes() {
        let sets = [
            bitmap_set(1, &[0, 1, 3]),
            bitmap_set(2, &[0, 2, 3]),
            bitmap_set(3, &[1, 2, 3]),
            bitmap_set(4, &[0, 1, 2]),
        ];
        let expr = expression(&sets);
        let expected = expr.cardinality().unwrap();
        let mut jit = DagJit::new();
        if generator_supported() {
            assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
            assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
            assert_eq!(jit.compiled_shapes(), 1);
        } else {
            assert!(jit.try_cardinality(&expr).is_none());
            assert_eq!(jit.compiled_shapes(), 0);
        }
        assert_eq!(cardinality(&expr).unwrap(), expected);
    }

    /// A set whose three prefixes are deliberately three different containers.
    ///
    /// `bitmap_set` draws 5 000 values per prefix and therefore only ever
    /// produces bitmaps, so nothing else in this module reaches `execute`'s
    /// unlendable branch. The kinds are asserted rather than assumed, because
    /// a later change to `ARRAY_MAX` or to `optimize` would otherwise turn this
    /// test back into another all-bitmap fixture without failing.
    fn three_shape_set(seed: u64) -> Arc<OrdSet> {
        let scattered = (0..5000u64).map(|low| {
            low.wrapping_mul(0x9e37_79b9)
                .wrapping_add(seed.wrapping_mul(0x85eb_ca6b))
                & 0xffff
        });
        let sparse = (0..50u64).map(|i| (1 << 16) | (i * 37 + seed % 17));
        let contiguous = (0..30_000u64).map(|i| (2 << 16) | i);
        let mut set = OrdSet::from_iter_unsorted(scattered.chain(sparse).chain(contiguous));
        set.optimize();
        let kinds: Vec<_> = set.chunks().map(|(_, c)| c.kind()).collect();
        assert_eq!(
            kinds,
            [
                ContainerKind::Bitmap,
                ContainerKind::Array,
                ContainerKind::Run
            ]
        );
        Arc::new(set)
    }

    #[test]
    fn array_and_run_prefixes_take_the_per_prefix_core_fallback() {
        // `execute` lends words to the kernel only where every live leaf at
        // that prefix is a full `BITMAP_WORDS` bitmap; prefix 1 is an array and
        // prefix 2 a run, so both take `generic_prefix` while prefix 0 is
        // fused. One expression therefore crosses the branch in both
        // directions, which is the shape a whole-prefix fixture cannot reach.
        let mixed = three_shape_set(3);
        let dense = bitmap_set(11, &[0, 1, 2]);
        let expr = Expr::set(mixed.clone())
            .and(Expr::set(dense.clone()))
            .or(Expr::set(dense).and_not(Expr::set(mixed)));
        let expected = expr.cardinality().unwrap();
        assert!(expected > 0);
        let mut jit = DagJit::new();
        if generator_supported() {
            assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
        } else {
            assert!(jit.try_cardinality(&expr).is_none());
        }
        assert_eq!(cardinality(&expr).unwrap(), expected);
    }

    #[test]
    fn unsupported_leaves_use_the_core_fallback() {
        let expr = Expr::set(bitmap_set(1, &[0])).and(Expr::Range(0, 100_000));
        let mut jit = DagJit::new();
        assert_eq!(jit.try_cardinality(&expr), None);
        assert_eq!(cardinality(&expr).unwrap(), expr.cardinality().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn jit_cache_drop_reclaims_executable_mappings() {
        const CHILD: &str = "YESNO_JIT_LIFETIME_CHILD";
        if !generator_supported() {
            return;
        }
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "jit::tests::jit_cache_drop_reclaims_executable_mappings",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        fn executable_mapping_bytes() -> usize {
            std::fs::read_to_string("/proc/self/maps")
                .unwrap()
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split_whitespace();
                    let range = fields.next()?;
                    if !fields.next()?.contains('x') {
                        return None;
                    }
                    let (start, end) = range.split_once('-')?;
                    Some(
                        usize::from_str_radix(end, 16).unwrap()
                            - usize::from_str_radix(start, 16).unwrap(),
                    )
                })
                .sum()
        }

        let sets = [
            bitmap_set(1, &[0]),
            bitmap_set(2, &[0]),
            bitmap_set(3, &[0]),
            bitmap_set(4, &[0]),
        ];
        let expr = expression(&sets);
        let expected = expr.cardinality().unwrap();
        let run = || {
            let mut jit = DagJit::new();
            assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
        };
        run(); // Warm up the compiler before measuring cache lifetime churn.
        let before = executable_mapping_bytes();
        for _ in 0..40 {
            run();
        }
        let after = executable_mapping_bytes();
        assert!(
            after <= before + 4096,
            "executable mappings grew by {} bytes over 40 dropped caches",
            after - before
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(8))]

        // Reclaiming one module must not invalidate another live kernel, and
        // a fresh module must still compile and run after its predecessor dies.
        #[test]
        fn module_reclamation_preserves_live_kernels(
            left_seed in any::<u16>(),
            right_seed in any::<u16>(),
        ) {
            let left = bitmap_set(u64::from(left_seed) + 1, &[0]);
            let right = bitmap_set(u64::from(right_seed) + 1, &[0]);
            let expr = Expr::set(left).and(Expr::set(right));
            let expected = expr.cardinality().unwrap();
            let mut first = DagJit::new();
            let mut second = DagJit::new();
            if generator_supported() {
                prop_assert_eq!(first.try_cardinality(&expr).unwrap().unwrap(), expected);
                prop_assert_eq!(second.try_cardinality(&expr).unwrap().unwrap(), expected);
                drop(first);
                prop_assert_eq!(second.try_cardinality(&expr).unwrap().unwrap(), expected);
                drop(second);
                let mut replacement = DagJit::new();
                prop_assert_eq!(replacement.try_cardinality(&expr).unwrap().unwrap(), expected);
            } else {
                prop_assert!(first.try_cardinality(&expr).is_none());
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        // This property guards both unsafe boundaries: the generated function
        // pointer's ABI and every bitmap pointer passed into it.
        #[test]
        fn generated_boolean_dags_match_the_core_oracle(
            seeds in prop::array::uniform4(any::<u16>()),
            prefix_masks in prop::array::uniform4(1u8..16),
            selector in any::<u8>(),
        ) {
            // Every leaf shares prefix zero so planning retains a compiled
            // DAG; independent remaining bits produce gaps and overlaps that
            // aligned-only generators would miss in the candidate walk.
            let positions = [0, 1, 128, 255];
            let sets: [Arc<OrdSet>; 4] = std::array::from_fn(|index| {
                let prefixes: Vec<_> = positions
                    .into_iter()
                    .enumerate()
                    .filter_map(|(bit, prefix)| {
                        ((prefix_masks[index] | 1) & (1 << bit) != 0).then_some(prefix)
                    })
                    .collect();
                bitmap_set(u64::from(seeds[index]) + 1, &prefixes)
            });
            let a = Expr::set(sets[0].clone());
            let b = Expr::set(sets[1].clone());
            let c = Expr::set(sets[2].clone());
            let d = Expr::set(sets[3].clone());
            let expr = match selector & 3 {
                0 => a.and(b).or(c.and_not(d)),
                1 => a.xor(b.or(c)).and_not(d),
                2 => a.or(b).xor(c.and(d)),
                _ => a.and_not(b.xor(c)).or(d),
            };
            let expected = expr.cardinality().unwrap();
            let actual = DagJit::new().try_cardinality(&expr);
            if generator_supported() {
                prop_assert_eq!(actual.unwrap().unwrap(), expected);
            } else {
                prop_assert!(actual.is_none());
                prop_assert_eq!(cardinality(&expr).unwrap(), expected);
            }
        }
    }
}

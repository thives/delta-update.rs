// Shared layout of the GPU search: every constant here is read by the shaders and, through
// `build.rs`, by the host. A line `#define NAME expression` becomes `pub const NAME: usize` in
// `gpu_layout`, so the two sides cannot drift. Only integer expressions of literals, other
// defines, `+ - * / % << >>` and parentheses are allowed, and every define must stay on one line.
//
// All device memory is addressed in 32-bit words. Image offsets and lengths are 32 bits, so an
// image of `MAX_IMAGE` bytes or more is refused by the host before anything is uploaded.

// ---- Search shape -------------------------------------------------------------------------
// The population keeps POP programs, the best ELITES of which are carried into the next generation
// unchanged. Each generation breeds CHILDREN more, each by its own thread, and the next population is
// the best POP of the elites and the children that were admitted. The children are many because they
// are bred, replayed and scored in parallel, so a wider brood costs little more time on a device with
// many threads.
#define POP 24
#define DEFAULT_ELITES 3
#define CHILDREN 96
// Candidates evaluated by one evaluation pass: the seeding generation evaluates this many, and the
// breeding generations evaluate CHILDREN. NCAND is the larger of CHILDREN and POP.
#define NCAND 96
// Pool slots: the population, and a slot for every candidate in flight.
#define POOL (POP + NCAND)
#define ARCHIVE 8
#define WORKGROUP 64

// ---- Chromosome capacities (workspace, not payload limits) --------------------------------
#define OP_CAP 512
#define EXT_CAP 16384
#define RUN_CAP 16384
#define MAX_IMAGE 1073741824
// Sums of lengths saturate here. A program that needs more is rejected as an overflow.
#define SUM_LIMIT 2147483647

// ---- Slot: one chromosome ------------------------------------------------------------------
// header | OP_CAP operation records | EXT_CAP extents. An operation record is
// {kind, dst_first, dst_count, src_first, src_count}. An extent is {start, length}. An
// operation's destination extents are contiguous from dst_first, and its source extents
// contiguous from src_first.
#define OPW 5
#define SLOT_HDR 16
#define SLOT_OPS SLOT_HDR
#define SLOT_EXT (SLOT_HDR + OP_CAP * OPW)
#define SLOT_WORDS (SLOT_EXT + EXT_CAP * 2)

#define H_NOPS 0
#define H_NEXT 1
// Raw payload size in bytes, or FIT_INVALID.
#define H_FIT 2
#define H_FP0 3
#define H_FP1 4
#define H_BLOB 5
#define H_STATUS 6
// Fingerprint of the bytes the program writes raw: two programs with the same one compress alike.
#define H_DFP0 7
#define H_DFP1 8
// Mismatch spans the repair covers, and the operation count before the repair.
#define H_MISM 9
#define H_RAWOPS 10
// Bytes the repair writes.
#define H_MISMBYTES 11
// Runs the replay left in the candidate's workspace.
#define H_RUNS 12
// Zero for non-mutations; otherwise the mutation ID plus one.
#define H_MOVE 13
// Changed cells the greedy seed entered the source-table matcher for. Only the greedy seed writes it.
#define H_SEARCHED 14

#define FIT_INVALID 4294967295u

#define K_REPLACE 0
#define K_ZERO 1
#define K_SOURCE 2
#define K_TARGET 3

// Evaluation outcome of a slot, kept in H_STATUS.
#define S_OK 0
#define S_SKIP 1
#define S_STRUCT 2
#define S_RUNCAP 3
#define S_OPCAP 4
#define S_EXTCAP 5
#define S_LIMIT 6
#define S_OVERFLOW 7
#define S_TARGET 8
#define S_ADMIT 9
#define S_REPAIR 10
#define S_WORK 11
#define S_COUNT 12

// Run moves and merged pieces one replay may spend. A replay that needs more is refused, so that no
// one candidate holds up the dispatch it runs in. A candidate with hundreds of extents costs a few
// thousand; one with thousands of unordered extents costs millions.
#define REPLAY_BUDGET 1048576
// Pairs of extents the compaction move compares to decide whether a copy may move to the front.
#define COMPACT_COMPARES 4096
// Most pieces an operation may have for the replay to sort them before merging them.
#define SORT_MAX 8192

// ---- Replay workspace ----------------------------------------------------------------------
// A run is one 128-bit vector {start, length, origin kind, origin offset}, in a buffer of their own,
// so moving a run is one load and one store.
// Each candidate has three run arrays: the replay writes into one, a bulk write merges into the
// second and swaps them, and the third holds the pieces a bulk write is about to merge.
#define RUN_SETS 3
#define RUN_VECS (NCAND * RUN_SETS * RUN_CAP)
#define O_OLD 0
#define O_NEW 1
#define O_ZERO 2

// Mismatch scan: the target is cut into at most SCAN_MAX equal ranges per candidate.
#define SCAN_MAX 1024
#define SCANW 6
#define SC_COUNT 0
#define SC_BYTES 1
#define SC_FIRSTOK 2
#define SC_BASE 3
#define SC_NEXTOK 4
#define NO_POS 4294967295u

// The old and the new image share one buffer, the old image first. `P_NEW_BASE` is the word at
// which the new image starts. Both are padded with whole zero words, so a word load that straddles
// the last byte stays inside the buffer. Keeping the images in one buffer means no stage ever
// chooses between two buffers at run time, which some compute devices handle badly.

// ---- State buffer --------------------------------------------------------------------------
#define PARAMS_WORDS 64
#define CTL_WORDS 128
#define ST_PARAMS 0
#define ST_CTL PARAMS_WORDS
#define ST_POP (ST_CTL + CTL_WORDS)
#define ST_FREE (ST_POP + POP)
#define ST_CAND (ST_FREE + POOL)
#define ST_SCAN (ST_CAND + NCAND)
#define ST_SLOTS (ST_SCAN + NCAND * SCAN_MAX * SCANW)
// Slots: the pool, then the archive, then two scratch slots per candidate.
#define SLOT_ARCH POOL
#define SLOT_SCR (POOL + ARCHIVE)
#define SLOTS (POOL + ARCHIVE + 2 * NCAND)
#define STATE_WORDS (ST_SLOTS + SLOTS * SLOT_WORDS)

// Parameter words, written once by the host.
#define P_OLD_LEN 0
#define P_NEW_LEN 1
#define P_SEED0 2
#define P_SEED1 3
#define P_MAX_OPS 4
#define P_MAX_EXT_OP 5
#define P_MAX_EXT_TOTAL 6
#define P_READABLE 7
#define P_SIG2 8
#define P_PART_FIXED 9
#define P_MAN_FIXED 10
#define P_MAX_MANIFEST 11
#define P_DEC_SLOT 12
#define P_DEC_HEAP 13
#define P_DEC_TRANS 14
#define P_MAX_DECODED 15
#define P_SCAN_RANGES 16
#define P_SCAN_LEN 17
#define P_NCELLS 18
#define P_NCHANGED 19
#define P_IDX_STRIDE 20
#define P_OLD_ENTRIES 21
#define P_NEW_ENTRIES 22
#define P_OLD_CAP 23
#define P_NEW_CAP 24
#define P_OLD_TABLE 25
#define P_NEW_TABLE 26
#define P_CLASSES 27
#define P_CHANGED 28
#define P_BLOCKS 29
#define P_HEADER 30
#define P_NEW_BASE 31
#define P_EQ_BITS 32
#define P_ZERO_BITS 33
#define P_TOURNAMENT 34
#define P_IMMIGRANT_PERCENT 35
#define P_CROSSOVER_PERCENT 36
#define P_ELITES 37

// Control words, written by the shaders and read by the host between batches.
#define C_GEN 0
#define C_BEST 1
#define C_NVALID 2
#define C_EVALS 3
#define C_ARCHIVED 4
#define C_BRED 5
#define C_SKIPPED 6
#define C_REJ 16
// Four saturating counters per mutation ID, updated by the selection thread.
#define C_MOVE 32
#define MOVE_STATS 4
#define MS_PROPOSED 0
#define MS_SKIPPED 1
#define MS_ADMITTED 2
#define MS_NEW_BEST 3

// ---- Index buffer --------------------------------------------------------------------------
// Sorted tables of {hash, position}, one per image, the per-cell classification, and two bit maps
// with a bit for every byte of the new image: one when the old image holds the same byte there, one
// when the new byte is zero. The offsets are parameters, since they depend on the image sizes.
#define PROBE 64
#define HASH_BASE 16777619
#define GRID 512
#define CELL_EQUAL 1
#define CELL_ZERO 2
#define BLOCK_CELLS 256
#define IDX_MIN_STRIDE 16
#define IDX_MAX_ENTRIES 4194304

// ---- Breeding ------------------------------------------------------------------------------
#define DEFAULT_TOURNAMENT 3
#define DEFAULT_IMMIGRANT_PERCENT 8
#define DEFAULT_CROSSOVER_PERCENT 12
#define CANDIDATES 32
#define TARGET_SHARE 30
#define GUIDED_SHARE 60
#define DATA_SHARE 60
#define SCAN_WINDOW 65536
// Bytes one proposal may compare in all, the longest run of zeros one zero proposal measures, and the
// source-table probes one proposal may make in all, hit or miss. A probe is a binary search of the table
// whether or not anything matches, so the lookups are bounded apart from the bytes they compare.
#define MAX_COMPARE 262144
#define MAX_ZERO_RUN 1048576
#define MAX_LOOKUPS 16384

#define MV_SPLIT 0
#define MV_MERGE 1
#define MV_SHIFT 2
#define MV_SHIFT_START 3
#define MV_COPY_CREATE 4
#define MV_SHORT_COPY 5
#define MV_COPY_SHIFT 6
#define MV_TARGET_CREATE 7
#define MV_ZERO_CREATE 8
#define MV_INSERT_ZERO 9
#define MV_REMOVE 10
#define MV_SWAP 11
#define MV_REGROUP 12
#define MV_REORDER 13
#define MV_SWEEP 14
#define MV_COMPACT 15
#define MV_COUNT 16

//! A transformer, running in ring 0 with no operating system beneath it.
//!
//! This is a full Llama-2 forward pass — RMSNorm, rotary embeddings, grouped
//! multi-head attention with a KV cache, SwiGLU feed-forward, and a final
//! projection over a 32,000-token vocabulary. The architecture is the same one
//! the large models use. Only the size is small.
//!
//! Nothing here is linked in. There is no BLAS, no libm, not even an allocator:
//! every matmul is the loop you see below, `exp` is a polynomial written out by
//! hand further down, and every buffer is a fixed-size static decided at
//! compile time. The weights arrive as a ramdisk the bootloader places in
//! memory, because a kernel with no filesystem has no other way to read 58MB.
//!
//! ## What it knows, and what it does not
//!
//! The weights are TinyStories-15M: 15 million parameters trained on simple
//! children's stories. It writes fluent, grammatical, often charming English.
//! It has never seen an operating system and does not know what a page fault
//! is. **Asking it about this kernel will produce confident nonsense.**
//!
//! That is the honest split in this OS, and both halves are deliberate:
//! `oracle.rs` is correct and cannot generalise; this is fluent and cannot be
//! trusted. Neither one is pretending to be the other.
//!
//! ## Format
//!
//! The legacy llama2.c export: a 28-byte header of seven i32 fields, then every
//! tensor as raw little-endian f32 in a fixed order, then precomputed RoPE
//! tables. We read the weights in place — 58MB is never copied.

use crate::println;
use core::arch::asm;

/// Model dimensions, read from the file rather than assumed.
#[derive(Clone, Copy)]
pub struct Config {
    pub dim: usize,
    pub hidden_dim: usize,
    pub n_layers: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub vocab_size: usize,
    pub seq_len: usize,
}

impl Config {
    fn head_size(&self) -> usize {
        self.dim / self.n_heads
    }
    fn kv_dim(&self) -> usize {
        (self.dim * self.n_kv_heads) / self.n_heads
    }
}

// Ceilings for the statically-allocated working memory. There is no allocator,
// so the buffers have to be sized at compile time; a model larger than these is
// rejected at load rather than silently corrupting memory.
const MAX_DIM: usize = 512;
const MAX_HIDDEN: usize = 1408;
const MAX_LAYERS: usize = 8;
const MAX_SEQ: usize = 256;
const MAX_VOCAB: usize = 32000;
const MAX_HEADS: usize = 8;

/// Everything the forward pass scribbles on. ~8MB of BSS.
struct RunState {
    x: [f32; MAX_DIM],
    xb: [f32; MAX_DIM],
    xb2: [f32; MAX_DIM],
    hb: [f32; MAX_HIDDEN],
    hb2: [f32; MAX_HIDDEN],
    q: [f32; MAX_DIM],
    att: [f32; MAX_HEADS * MAX_SEQ],
    logits: [f32; MAX_VOCAB],
    key_cache: [f32; MAX_LAYERS * MAX_SEQ * MAX_DIM],
    value_cache: [f32; MAX_LAYERS * MAX_SEQ * MAX_DIM],
}

/// The one set of activations. Every access is gated by `GENERATING` above,
/// which is what makes the `addr_of_mut!` reads below exclusive rather than
/// merely unobserved.
static mut STATE: RunState = RunState {
    x: [0.0; MAX_DIM],
    xb: [0.0; MAX_DIM],
    xb2: [0.0; MAX_DIM],
    hb: [0.0; MAX_HIDDEN],
    hb2: [0.0; MAX_HIDDEN],
    q: [0.0; MAX_DIM],
    att: [0.0; MAX_HEADS * MAX_SEQ],
    logits: [0.0; MAX_VOCAB],
    key_cache: [0.0; MAX_LAYERS * MAX_SEQ * MAX_DIM],
    value_cache: [0.0; MAX_LAYERS * MAX_SEQ * MAX_DIM],
};

/// Views into the weights, pointing directly at ramdisk memory.
struct Weights {
    token_embedding: &'static [f32],
    rms_att: &'static [f32],
    wq: &'static [f32],
    wk: &'static [f32],
    wv: &'static [f32],
    wo: &'static [f32],
    rms_ffn: &'static [f32],
    w1: &'static [f32],
    w2: &'static [f32],
    w3: &'static [f32],
    rms_final: &'static [f32],
    freq_cos: &'static [f32],
    freq_sin: &'static [f32],
}

/// One vocabulary entry, borrowed from the ramdisk.
#[derive(Clone, Copy)]
struct Token {
    text: &'static str,
    score: f32,
}

static mut VOCAB: [Token; MAX_VOCAB] = [Token { text: "", score: 0.0 }; MAX_VOCAB];

struct Model {
    config: Config,
    weights: Weights,
    vocab_len: usize,
}

static mut MODEL: Option<Model> = None;

fn model() -> Option<&'static Model> {
    unsafe { (*core::ptr::addr_of!(MODEL)).as_ref() }
}

// --- loading -------------------------------------------------------------

/// Parse the ramdisk the bootloader placed in memory.
///
/// # Safety
/// `base` must point at `len` readable bytes that stay valid forever, which is
/// exactly what a bootloader ramdisk is.
pub unsafe fn init(base: *const u8, len: usize) {
    if len < 20 {
        return;
    }
    let blob = unsafe { core::slice::from_raw_parts(base, len) };

    if read_u32(blob, 0) != 0x354D_4C4D {
        println!("[llm ] ramdisk is not a 5AM-OS model blob -- ignoring it.");
        return;
    }
    let model_len = read_u64(blob, 4) as usize;
    let tok_len = read_u64(blob, 12) as usize;
    if 20 + model_len + tok_len > len {
        println!("[llm ] ramdisk is truncated -- ignoring it.");
        return;
    }

    let model_bytes = &blob[20..20 + model_len];
    let tokenizer_bytes = &blob[20 + model_len..20 + model_len + tok_len];

    let Some((config, weights)) = (unsafe { parse_model(model_bytes) }) else {
        return;
    };
    let vocab_len = unsafe { parse_tokenizer(tokenizer_bytes, config.vocab_size) };

    unsafe {
        MODEL = Some(Model { config, weights, vocab_len });
    }
}

unsafe fn parse_model(bytes: &'static [u8]) -> Option<(Config, Weights)> {
    if bytes.len() < 28 {
        return None;
    }
    let config = Config {
        dim: read_i32(bytes, 0) as usize,
        hidden_dim: read_i32(bytes, 4) as usize,
        n_layers: read_i32(bytes, 8) as usize,
        n_heads: read_i32(bytes, 12) as usize,
        n_kv_heads: read_i32(bytes, 16) as usize,
        // A negative vocab size is llama2.c's flag for an unshared classifier.
        // This model shares it with the embedding table, so it is positive.
        vocab_size: read_i32(bytes, 20).unsigned_abs() as usize,
        seq_len: read_i32(bytes, 24) as usize,
    };

    if config.dim > MAX_DIM
        || config.hidden_dim > MAX_HIDDEN
        || config.n_layers > MAX_LAYERS
        || config.seq_len > MAX_SEQ
        || config.vocab_size > MAX_VOCAB
        || config.n_heads > MAX_HEADS
    {
        println!("[llm ] model is larger than the kernel's static buffers.");
        println!("       Raise the MAX_* constants in llm.rs and rebuild.");
        return None;
    }

    // The tensors follow the header back to back, in this exact order.
    let floats: &'static [f32] = unsafe {
        core::slice::from_raw_parts(
            bytes.as_ptr().add(28) as *const f32,
            (bytes.len() - 28) / 4,
        )
    };

    let (dim, hidden, layers) = (config.dim, config.hidden_dim, config.n_layers);
    let (heads, kv_dim, head_size) = (config.n_heads, config.kv_dim(), config.head_size());
    let vocab = config.vocab_size;

    let mut at = 0usize;
    let mut take = |count: usize| -> &'static [f32] {
        let slice = &floats[at..at + count];
        at += count;
        slice
    };

    let weights = Weights {
        token_embedding: take(vocab * dim),
        rms_att: take(layers * dim),
        wq: take(layers * dim * heads * head_size),
        wk: take(layers * dim * kv_dim),
        wv: take(layers * dim * kv_dim),
        wo: take(layers * heads * head_size * dim),
        rms_ffn: take(layers * dim),
        w1: take(layers * hidden * dim),
        w2: take(layers * dim * hidden),
        w3: take(layers * hidden * dim),
        rms_final: take(dim),
        // The legacy export ships precomputed RoPE tables. Using them means the
        // kernel needs no sin/cos implementation at all.
        freq_cos: take(config.seq_len * head_size / 2),
        freq_sin: take(config.seq_len * head_size / 2),
    };

    Some((config, weights))
}

/// tokenizer.bin: max_token_length (u32), then per token a score (f32),
/// a length (u32), and that many bytes.
unsafe fn parse_tokenizer(bytes: &'static [u8], vocab_size: usize) -> usize {
    let mut at = 4usize;
    let mut count = 0usize;

    while count < vocab_size && at + 8 <= bytes.len() {
        let score = f32::from_bits(read_u32(bytes, at));
        let len = read_u32(bytes, at + 4) as usize;
        at += 8;
        if at + len > bytes.len() {
            break;
        }
        let text = core::str::from_utf8(&bytes[at..at + len]).unwrap_or("");
        at += len;
        unsafe {
            (*core::ptr::addr_of_mut!(VOCAB))[count] = Token { text, score };
        }
        count += 1;
    }
    count
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
fn read_i32(bytes: &[u8], at: usize) -> i32 {
    read_u32(bytes, at) as i32
}
fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut value = 0u64;
    for i in 0..8 {
        value |= (bytes[at + i] as u64) << (8 * i);
    }
    value
}

// --- math ----------------------------------------------------------------

/// Square root, as a single hardware instruction.
pub fn sqrt(x: f32) -> f32 {
    let result: f32;
    unsafe {
        asm!("sqrtss {0}, {1}", out(xmm_reg) result, in(xmm_reg) x, options(nomem, nostack));
    }
    result
}

/// e^x, written out because there is no libm to link against.
///
/// The trick is to move the work into the exponent field of the float itself:
/// e^x = 2^(x·log2 e), and 2^n for integer n is just a bit pattern. Only the
/// fractional part needs a polynomial, and only over [-0.5, 0.5], where a
/// degree-5 minimax fit is accurate to about a single-precision ulp.
pub fn exp(x: f32) -> f32 {
    if x < -87.0 {
        return 0.0;
    }
    if x > 88.0 {
        return f32::INFINITY;
    }

    const LOG2_E: f32 = 1.442_695_f32;
    let t = x * LOG2_E;

    // Round to nearest integer, without libm's roundf.
    let n = if t >= 0.0 { (t + 0.5) as i32 } else { (t - 0.5) as i32 };
    let f = t - n as f32;

    // 2^f on [-0.5, 0.5], Horner form.
    let p = 1.0
        + f * (0.693_147_18
            + f * (0.240_226_51
                + f * (0.055_504_11 + f * (0.009_618_13 + f * 0.001_332_70))));

    // Multiply by 2^n by constructing the float directly.
    let scale = f32::from_bits(((n + 127) as u32) << 23);
    p * scale
}

fn rmsnorm(out: &mut [f32], x: &[f32], weight: &[f32], size: usize) {
    let mut sum = 0.0f32;
    for i in 0..size {
        sum += x[i] * x[i];
    }
    // The normalisation the whole architecture is named for: scale by the root
    // mean square, with an epsilon so an all-zero vector does not divide by 0.
    let scale = 1.0 / sqrt(sum / size as f32 + 1e-5);
    for i in 0..size {
        out[i] = weight[i] * (x[i] * scale);
    }
}

fn softmax(x: &mut [f32]) {
    let mut max = x[0];
    for &value in x.iter() {
        if value > max {
            max = value;
        }
    }
    // Subtracting the max before exponentiating is what keeps this from
    // overflowing to infinity on a confident distribution.
    let mut sum = 0.0f32;
    for value in x.iter_mut() {
        *value = exp(*value - max);
        sum += *value;
    }
    for value in x.iter_mut() {
        *value /= sum;
    }
}

/// out = W·x, where W is (d × n) stored row-major.
///
/// This single loop is where essentially all the time goes — for this model it
/// runs about 15 million multiply-adds per token generated.
fn matmul(out: &mut [f32], x: &[f32], w: &[f32], n: usize, d: usize) {
    for i in 0..d {
        let row = &w[i * n..i * n + n];
        let mut sum = 0.0f32;
        for j in 0..n {
            sum += row[j] * x[j];
        }
        out[i] = sum;
    }
}

// --- the forward pass ----------------------------------------------------

fn forward(model: &Model, token: usize, pos: usize) -> &'static mut [f32] {
    let config = model.config;
    let w = &model.weights;
    let (dim, hidden, kv_dim, head_size) =
        (config.dim, config.hidden_dim, config.kv_dim(), config.head_size());
    let kv_mul = config.n_heads / config.n_kv_heads;
    let state = unsafe { &mut *core::ptr::addr_of_mut!(STATE) };

    // Look the token up in the embedding table. This is the only place the
    // input text enters the network.
    state.x[..dim].copy_from_slice(&w.token_embedding[token * dim..token * dim + dim]);

    for layer in 0..config.n_layers {
        // --- attention ---
        rmsnorm(&mut state.xb, &state.x, &w.rms_att[layer * dim..], dim);

        // Q, K and V are three different projections of the same input.
        let key_offset = layer * config.seq_len * kv_dim + pos * kv_dim;
        matmul(&mut state.q, &state.xb[..dim], &w.wq[layer * dim * dim..], dim, dim);
        {
            let (keys, values) = (&mut state.key_cache, &mut state.value_cache);
            let k = &mut keys[key_offset..key_offset + kv_dim];
            matmul(k, &state.xb[..dim], &w.wk[layer * dim * kv_dim..], dim, kv_dim);
            let v = &mut values[key_offset..key_offset + kv_dim];
            matmul(v, &state.xb[..dim], &w.wv[layer * dim * kv_dim..], dim, kv_dim);
        }

        // Rotary position embedding: rotate each pair of dimensions by an angle
        // that depends on the position. This is how the model knows word order
        // without any positional vector being added anywhere.
        let half = head_size / 2;
        for head in 0..config.n_heads {
            for i in 0..half {
                let (cos, sin) = (w.freq_cos[pos * half + i], w.freq_sin[pos * half + i]);
                let base = head * head_size + i * 2;
                let (a, b) = (state.q[base], state.q[base + 1]);
                state.q[base] = a * cos - b * sin;
                state.q[base + 1] = a * sin + b * cos;
            }
        }
        for head in 0..config.n_kv_heads {
            for i in 0..half {
                let (cos, sin) = (w.freq_cos[pos * half + i], w.freq_sin[pos * half + i]);
                let base = key_offset + head * head_size + i * 2;
                let (a, b) = (state.key_cache[base], state.key_cache[base + 1]);
                state.key_cache[base] = a * cos - b * sin;
                state.key_cache[base + 1] = a * sin + b * cos;
            }
        }

        // --- multi-head attention over everything seen so far ---
        let scale = 1.0 / sqrt(head_size as f32);
        for head in 0..config.n_heads {
            let q = &state.q[head * head_size..head * head_size + head_size];
            let att = &mut state.att[head * MAX_SEQ..head * MAX_SEQ + pos + 1];

            for t in 0..=pos {
                let k_at = layer * config.seq_len * kv_dim
                    + t * kv_dim
                    + (head / kv_mul) * head_size;
                let mut score = 0.0f32;
                for i in 0..head_size {
                    score += q[i] * state.key_cache[k_at + i];
                }
                att[t] = score * scale;
            }
            softmax(att);

            // Weighted sum of the values — the actual "attending".
            let out = &mut state.xb[head * head_size..head * head_size + head_size];
            out.fill(0.0);
            for t in 0..=pos {
                let v_at = layer * config.seq_len * kv_dim
                    + t * kv_dim
                    + (head / kv_mul) * head_size;
                let weight = att[t];
                for i in 0..head_size {
                    out[i] += weight * state.value_cache[v_at + i];
                }
            }
        }

        matmul(&mut state.xb2, &state.xb[..dim], &w.wo[layer * dim * dim..], dim, dim);
        for i in 0..dim {
            state.x[i] += state.xb2[i];
        }

        // --- feed-forward (SwiGLU) ---
        rmsnorm(&mut state.xb, &state.x, &w.rms_ffn[layer * dim..], dim);
        matmul(&mut state.hb, &state.xb[..dim], &w.w1[layer * hidden * dim..], dim, hidden);
        matmul(&mut state.hb2, &state.xb[..dim], &w.w3[layer * hidden * dim..], dim, hidden);
        for i in 0..hidden {
            // SiLU: x·sigmoid(x), gated by the parallel w3 projection.
            let value = state.hb[i];
            state.hb[i] = value * (1.0 / (1.0 + exp(-value))) * state.hb2[i];
        }
        matmul(&mut state.xb, &state.hb[..hidden], &w.w2[layer * dim * hidden..], hidden, dim);
        for i in 0..dim {
            state.x[i] += state.xb[i];
        }
    }

    let x_copy = state.x;
    rmsnorm(&mut state.x, &x_copy, w.rms_final, dim);
    // Shared weights: the embedding table doubles as the output classifier.
    matmul(
        &mut state.logits,
        &state.x[..dim],
        w.token_embedding,
        dim,
        config.vocab_size,
    );

    unsafe { &mut (*core::ptr::addr_of_mut!(STATE)).logits }
}

// --- tokenizer -----------------------------------------------------------

fn lookup(model: &Model, text: &str) -> Option<usize> {
    let vocab = unsafe { &*core::ptr::addr_of!(VOCAB) };
    (0..model.vocab_len).find(|&i| vocab[i].text == text)
}

/// Byte-pair encoding: start from single characters, then repeatedly merge the
/// highest-scoring adjacent pair that exists in the vocabulary.
fn encode(model: &Model, text: &str, tokens: &mut [usize]) -> usize {
    let vocab = unsafe { &*core::ptr::addr_of!(VOCAB) };
    let mut count = 0usize;
    let mut buffer = [0u8; 4];

    // Llama's tokenizer is trained with a leading space on every input, so the
    // prompt gets one whether the user typed it or not. Without this, the first
    // word is tokenized differently than the model ever saw in training.
    //
    // Note this tokenizer.bin stores real spaces: llama2.c's export script
    // already rewrote sentencepiece's U+2581 marker back to ' '. Encoding the
    // marker instead sends every space down the byte-fallback path, which is a
    // mistake that survives all the way to visible mojibake in the output.
    for ch in core::iter::once(' ').chain(text.chars()) {
        if count >= tokens.len() {
            break;
        }
        let piece: &str = ch.encode_utf8(&mut buffer);
        match lookup(model, piece) {
            Some(id) => {
                tokens[count] = id;
                count += 1;
            }
            None => {
                // Byte fallback: raw bytes occupy ids 3..259.
                for byte in piece.as_bytes() {
                    if count < tokens.len() {
                        tokens[count] = *byte as usize + 3;
                        count += 1;
                    }
                }
            }
        }
    }

    // Merge until nothing improves.
    let mut scratch = [0u8; 64];
    loop {
        let mut best: Option<(f32, usize, usize)> = None;
        for i in 0..count.saturating_sub(1) {
            let (a, b) = (vocab[tokens[i]].text, vocab[tokens[i + 1]].text);
            let len = a.len() + b.len();
            if len > scratch.len() {
                continue;
            }
            scratch[..a.len()].copy_from_slice(a.as_bytes());
            scratch[a.len()..len].copy_from_slice(b.as_bytes());
            let Ok(joined) = core::str::from_utf8(&scratch[..len]) else {
                continue;
            };
            if let Some(id) = lookup(model, joined) {
                let score = vocab[id].score;
                if best.map_or(true, |(b, _, _)| score > b) {
                    best = Some((score, i, id));
                }
            }
        }

        let Some((_, at, id)) = best else { break };
        tokens[at] = id;
        for i in at + 1..count - 1 {
            tokens[i] = tokens[i + 1];
        }
        count -= 1;
    }

    count
}

/// Print one token's text, handling the two encoding conventions.
fn emit(model: &Model, token: usize, previous: usize) {
    let vocab = unsafe { &*core::ptr::addr_of!(VOCAB) };
    if token >= model.vocab_len {
        return;
    }
    // Token 1 is beginning-of-sequence; its text is literally "\n<s>\n" and
    // printing it would put a stray marker in front of every generation.
    if token == 1 {
        return;
    }
    let mut text = vocab[token].text;

    // After a beginning-of-sequence token, a leading space is stripped.
    if previous == 1 {
        text = text.trim_start_matches(' ');
    }

    // Byte-fallback tokens are spelled "<0xXX>".
    if let Some(hex) = text.strip_prefix("<0x").and_then(|s| s.strip_suffix('>')) {
        if let Ok(byte) = u8::from_str_radix(hex, 16) {
            crate::print!("{}", byte as char);
            return;
        }
    }

    crate::print!("{text}");
}

// --- generation ----------------------------------------------------------

/// Greedy decoding: always take the most likely next token.
///
/// Deterministic, which makes it reproducible and easy to debug. Sampling with
/// a temperature would produce more varied text and is a small change to make.
fn argmax(values: &[f32]) -> usize {
    let mut best = 0usize;
    for (i, &value) in values.iter().enumerate() {
        if value > values[best] {
            best = i;
        }
    }
    best
}

/// Generate from a prompt, streaming each token as it is produced.
/// Whoever is currently using STATE.
///
/// There is exactly one set of activations, key cache and value cache in this
/// kernel -- several megabytes of statics, sized for the largest model it
/// supports. Once tasks became preemptible, two `spawn`s would take turns
/// writing into the same buffers and produce fluent nonsense, with nothing
/// anywhere reporting a problem.
///
/// The cheap fix would be one generation at a time by convention. This makes it
/// true.
static GENERATING: crate::sync::Claim = crate::sync::Claim::new();

/// Is a generation in progress? For the shell's `tasks` output.
pub fn busy() -> bool {
    GENERATING.is_taken()
}

pub fn generate(prompt: &str, steps: usize) {
    // Held for the whole run and released on every exit path, including the
    // early return just below.
    let Some(_generating) = GENERATING.try_take() else {
        println!();
        println!("The model is already running somewhere else.");
        println!("There is one set of activations in this kernel, so a second");
        println!("generation would write into the first one's KV cache and both");
        println!("would produce confident nonsense. Try again when it finishes.");
        return;
    };

    let Some(model) = model() else {
        println!("No model is loaded.");
        println!();
        println!("The weights ship as a ramdisk, and this image was built");
        println!("without one. See the README section `the neural network`.");
        return;
    };

    let mut tokens = [0usize; 128];
    // Token 1 is beginning-of-sequence.
    tokens[0] = 1;
    let prompt_len = encode(model, prompt, &mut tokens[1..]) + 1;

    let limit = steps.min(model.config.seq_len);
    println!();

    let start = crate::interrupts::ticks();
    let mut token = tokens[0];
    let mut previous = 0usize;
    let mut generated = 0usize;

    for pos in 0..limit {
        let logits = forward(model, token, pos);

        let next = if pos + 1 < prompt_len {
            // Still replaying the prompt: feed the known token, ignore the
            // prediction. The forward pass still runs, because the KV cache
            // has to be filled in for those positions.
            tokens[pos + 1]
        } else {
            argmax(logits)
        };

        emit(model, token, previous);
        previous = token;
        token = next;
        generated += 1;

        // Token 2 is end-of-sequence.
        if token == 2 {
            break;
        }
    }
    emit(model, token, previous);

    let elapsed = crate::interrupts::ticks().saturating_sub(start);
    println!();
    println!();
    println!(
        "[llm ] {generated} tokens in ~{} s ({} ticks) -- {} params, {} layers",
        elapsed / 18,
        elapsed,
        "15M",
        model.config.n_layers,
    );
}

/// Describe the loaded model, for the shell.
pub fn describe() {
    match model() {
        None => {
            println!("  No model loaded. This image was built without a ramdisk.");
            println!("  See the README section `the neural network`.");
        }
        Some(model) => {
            let c = model.config;
            println!("  A Llama-2 transformer, running in ring 0.");
            println!();
            println!("    dim         {}", c.dim);
            println!("    hidden      {}", c.hidden_dim);
            println!("    layers      {}", c.n_layers);
            println!("    heads       {}  ({} kv)", c.n_heads, c.n_kv_heads);
            println!("    head size   {}", c.head_size());
            println!("    vocab       {} tokens ({} parsed)", c.vocab_size, model.vocab_len);
            println!("    context     {} tokens", c.seq_len);
            println!();
            println!("  Weights are read straight out of the ramdisk -- 58MB that is");
            println!("  never copied, because there is nowhere to copy it to.");
            println!();
            println!("  It was trained on children's stories. It writes fluent English");
            println!("  and knows nothing whatsoever about this kernel: ask it about a");
            println!("  page fault and it will invent something confident and wrong.");
            println!("  Use `ask` for questions about the machine.");
        }
    }
}

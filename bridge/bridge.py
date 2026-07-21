#!/usr/bin/env python3
"""The other end of 5AM-OS's serial wire.

The kernel has no network stack, no filesystem, and no allocator, so it cannot
run a language model. What it *can* do is write bytes to a UART. This program
reads those bytes, asks Claude, and writes the answer back down the same wire.

The kernel does not know or care that a model is involved — it sent a question
and got text back. The day 5AM-OS grows a virtio-net driver, this process is
deleted and the kernel calls the API itself.

Usage
-----
    export ANTHROPIC_API_KEY=...
    python3 bridge/bridge.py

Then, inside 5AM-OS:

    5am> ask why is CR3 different from the address my kernel was loaded at?

Wire protocol (see kernel/src/ai.rs)
------------------------------------
    <- 5AMOS/1 ASK | 5AMOS/1 FAULT
    <- state: cr0=0x... cr3=0x... ring=0 ticks=42 [fault=... rip=... cr2=...]
    <- q: <question>
    <- END
    -> <answer lines>
    -> END
"""

from __future__ import annotations

import os
import socket
import sys

try:
    import anthropic
except ImportError:
    sys.exit(
        "The anthropic SDK is not installed.\n"
        "    pip install -r bridge/requirements.txt"
    )

HOST = os.environ.get("BRIDGE_HOST", "127.0.0.1")
PORT = int(os.environ.get("BRIDGE_PORT", "4444"))
MODEL = os.environ.get("BRIDGE_MODEL", "claude-opus-5")

# The kernel prints the reply on an 80-column serial console with no wrapping,
# so the model is asked to wrap for us. It is also told what it is talking to —
# without that, answers drift toward generic OS advice instead of addressing the
# actual register values in front of it.
SYSTEM_PROMPT = """You are attached to a hobby x86_64 kernel called 5AM-OS over \
a serial port. It is written in Rust, runs in QEMU, and exists to teach its \
author how operating systems work.

What it has, all written by hand in this repository: a 16550 serial console and \
a framebuffer console; its own GDT with a TSS and an IST stack; an IDT with CPU \
exception handlers; a remapped 8259 PIC and a PIT timer; a PS/2 keyboard \
driver; a physical frame allocator and its own four-level page table code; a \
linked-list heap behind Vec and Box; preemptive multitasking with blocked and \
sleeping states, priorities and aging; ring 3 with int 0x80 and thirteen \
syscalls; per-process address spaces; fork with copy-on-write; exec, wait, \
pipes and file descriptors; signals delivered by writing a frame onto the \
program's own stack; demand-paged stacks and swapping to raw disk blocks; an \
ELF loader; ATA PIO and a read-write FAT16 driver; a shell that runs in ring 3 \
as the first user process; a 15M-parameter transformer running in ring 0; and \
self-tests that run inside the machine. The scheduler and the page replacement \
algorithm are swappable at runtime -- five and four implementations \
respectively -- so do not assume which one is installed.

What it does NOT have: directories (FAT16 root only, 8.3 names), open/close or \
file offsets, mmap, a page cache, a tty layer or process groups, real \
timekeeping beyond timer ticks, dynamic linking, userspace threads, working \
SMP (a second core is woken and parked, but the kernel is not safe for it), or \
any network stack -- which is why you are reached over a serial port rather \
than called directly.

If a question assumes a feature from that second list, say so rather than \
answering as though it exists.

You will be given the machine's live register state and a question. Answer the \
question using those specific values — cite the actual numbers rather than \
describing registers in the abstract.

Format for a dumb terminal:
- Hard-wrap at 72 columns.
- Plain ASCII only. No markdown, no bold, no bullets beyond "- ".
- Be direct and concrete. Six lines is usually better than twenty.
- If the state does not contain enough information to answer, say so plainly \
and name what would be needed."""


def read_request(conn: socket.socket) -> tuple[str, str, str] | None:
    """Read one framed request. Returns (verb, state, question), or None on EOF."""
    buffer = b""
    while b"\nEND\n" not in buffer.replace(b"\r", b""):
        chunk = conn.recv(4096)
        if not chunk:
            return None
        buffer += chunk

    lines = buffer.replace(b"\r", b"").decode("utf-8", "replace").split("\n")

    verb, state, question = "ASK", "", ""
    for line in lines:
        if line.startswith("5AMOS/1 "):
            verb = line.removeprefix("5AMOS/1 ").strip()
        elif line.startswith("state:"):
            state = line
        elif line.startswith("q: "):
            question = line.removeprefix("q: ")
        elif line == "END":
            break
    return verb, state, question


def answer(client: anthropic.Anthropic, verb: str, state: str, question: str) -> str:
    prompt = f"{state}\n\nQuestion: {question}"
    if verb == "FAULT":
        prompt = (
            f"{state}\n\nThe kernel just took a fault and is about to halt. "
            f"{question}"
        )

    # Streaming, because a thorough answer to a fault question can be long
    # enough to risk an HTTP timeout on a non-streaming call.
    with client.messages.stream(
        model=MODEL,
        max_tokens=16000,
        system=SYSTEM_PROMPT,
        messages=[{"role": "user", "content": prompt}],
    ) as stream:
        message = stream.get_final_message()

    return "".join(block.text for block in message.content if block.type == "text")


def send_reply(conn: socket.socket, text: str) -> None:
    # The kernel treats a lone "END" line as the terminator, so any such line in
    # the body would truncate the answer mid-sentence.
    safe = "\n".join(" END" if line.strip() == "END" else line for line in text.split("\n"))
    conn.sendall(safe.encode("utf-8", "replace") + b"\nEND\n")


def main() -> None:
    if not os.environ.get("ANTHROPIC_API_KEY"):
        print("ANTHROPIC_API_KEY is not set — the bridge has nothing to ask.")
        print("Set it in this shell, then start the bridge again.")
        sys.exit(1)

    client = anthropic.Anthropic()

    with socket.create_server((HOST, PORT), reuse_port=False) as server:
        print(f"5AM-OS bridge listening on {HOST}:{PORT} (model: {MODEL})")
        print("Boot the OS with ./run.sh, then type `ask <question>` in its shell.")
        print("Ctrl-C to stop.\n")

        while True:
            conn, _ = server.accept()
            with conn:
                print("[bridge] kernel connected")
                try:
                    while True:
                        request = read_request(conn)
                        if request is None:
                            break
                        verb, state, question = request
                        print(f"[bridge] {verb}: {question[:70]}")
                        try:
                            reply = answer(client, verb, state, question)
                        except anthropic.APIStatusError as exc:
                            reply = f"The bridge could not reach the API: {exc.message}"
                        except anthropic.APIConnectionError:
                            reply = "The bridge has no network connection."
                        send_reply(conn, reply)
                except (ConnectionResetError, BrokenPipeError):
                    pass
                print("[bridge] kernel disconnected")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nbridge stopped")

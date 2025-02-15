This file is on a disk.

That sounds like nothing. It is the first time in this project that it is
true. Everything else 5AM-OS has ever run was compiled into the kernel:
the shell, the neural network weights, even the ring 3 program, all baked
into one image at build time. include_bytes! is not a filesystem. It is a
promise made by the compiler.

The kernel found this file by reading sector 0 of a disk it had never seen,
believing what the BIOS Parameter Block told it about where things live,
walking the root directory, and following a chain of cluster numbers
through the File Allocation Table.

None of those bytes were arranged by anyone who knew this kernel existed.
mkfs wrote a FAT16 volume, and FAT16 is documented well enough that the
image mounts on macOS, Windows and Linux too. Try it:

    hdiutil attach target/fs.img

If Finder can open it and 5AM-OS can read it, both of us understood the
same specification. That is the only real test of a filesystem reader.

    -- 5AM-OS

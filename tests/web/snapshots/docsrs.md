title: Rope in ropey - Rust
read: main
charset: UTF-8
links: 753
anchors: 225
---
[ropey](https://docs.rs/ropey/latest/ropey/index.html)

# Struct Rope

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#82-84)

```rust
pub struct Rope { /* private fields */ }
```

Expand description

A utf8 text rope.

The time complexity of nearly all edit and query operations on `Rope` are worst-case `O(log N)` in the length of the rope. `Rope` is designed to work efficiently even for huge (in the gigabytes) and pathological (all on one line) texts.

## Editing Operations

The primary editing operations on `Rope` are insertion and removal of text. For example:

```rust
let mut rope = Rope::from_str("Hello みんなさん!");
rope.remove(6..11);
rope.insert(6, "world");

assert_eq!(rope, "Hello world!");
```

## Query Operations

`Rope` provides a rich set of efficient query functions, including querying rope length in bytes/`char`s/lines, fetching individual `char`s or lines, and converting between byte/`char`/line indices. For example, to find the starting `char` index of a given line:

```rust
let rope = Rope::from_str("Hello みんなさん!\nHow are you?\nThis text has multiple lines!");

assert_eq!(rope.line_to_char(0), 0);
assert_eq!(rope.line_to_char(1), 13);
assert_eq!(rope.line_to_char(2), 26);
```

## Slicing

You can take immutable slices of a `Rope` using `slice()`:

```rust
let mut rope = Rope::from_str("Hello みんなさん!");
let middle = rope.slice(3..8);

assert_eq!(middle, "lo みん");
```

## Cloning

Cloning `Rope`s is extremely cheap, running in `O(1)` time and taking a small constant amount of memory for the new clone, regardless of text size. This is accomplished by data sharing between `Rope` clones. The memory used by clones only grows incrementally as the their contents diverge due to edits. All of this is thread safe, so clones can be sent freely between threads.

The primary intended use-case for this feature is to allow asynchronous processing of `Rope`s. For example, saving a large document to disk in a separate thread while the user continues to perform edits.

## Implementations

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#86-1259)

### impl [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#92-96)

#### pub fn new() -> Self

Creates an empty `Rope`.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#103-105)

#### pub fn from_str(text: &[str](https://doc.rust-lang.org/nightly/std/primitive.str.html)) -> Self

Creates a `Rope` from a string slice.

Runs in O(N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#129-192)

#### pub fn from_reader<T: [Read](https://doc.rust-lang.org/nightly/alloc/io/read/trait.Read.html)>(reader: T) -> [Result](https://doc.rust-lang.org/nightly/core/io/error/type.Result.html)<Self>

Creates a `Rope` from the output of a reader.

This is a convenience function, and provides *no specific guarantees* about performance or internal implementation aside from the runtime complexity listed below.

When more precise control over IO behavior, buffering, etc. is desired, you should handle IO yourself and use [`RopeBuilder`](https://docs.rs/ropey/latest/ropey/struct.RopeBuilder.html) to build the `Rope`.

Runs in O(N) time.

##### Errors

- If the reader returns an error, `from_reader` stops and returns that error.
- If non-utf8 data is encountered, an IO error with kind `InvalidData` is returned.

Note: some data from the reader is likely consumed even if there is an error.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#216-222)

#### pub fn write_to<T: [Write](https://doc.rust-lang.org/nightly/core/io/write/trait.Write.html)>(&self, writer: T) -> [Result](https://doc.rust-lang.org/nightly/core/io/error/type.Result.html)<[()](https://doc.rust-lang.org/nightly/std/primitive.unit.html)>

Writes the contents of the `Rope` to a writer.

This is a convenience function, and provides *no specific guarantees* about performance or internal implementation aside from the runtime complexity listed below.

When more precise control over IO behavior, buffering, etc. is desired, you should handle IO yourself and use the [`Chunks`](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html) iterator to iterate through the `Rope`’s contents.

Runs in O(N) time.

##### Errors

- If the writer returns an error, `write_to` stops and returns that error.

Note: some data may have been written even if an error is returned.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#231-233)

#### pub fn len_bytes(&self) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Total number of bytes in the `Rope`.

Runs in O(1) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#239-241)

#### pub fn len_chars(&self) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Total number of chars in the `Rope`.

Runs in O(1) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#247-249)

#### pub fn len_lines(&self) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Total number of lines in the `Rope`.

Runs in O(1) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#261-264)

#### pub fn len_utf16_cu(&self) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Total number of utf16 code units that would be in `Rope` if it were encoded as utf16.

Ropey stores text internally as utf8, but sometimes it is necessary to interact with external APIs that still use utf16. This function is primarily intended for such situations, and is otherwise not very useful.

Runs in O(1) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#276-282)

#### pub fn capacity(&self) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Total size of the `Rope`’s text buffer space, in bytes.

This includes unoccupied text buffer space. You can calculate the unoccupied space with `capacity() - len_bytes()`. In general, there will always be some unoccupied buffer space.

Runs in O(N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#303-329)

#### pub fn shrink_to_fit(&mut self)

Shrinks the `Rope`’s capacity to the minimum possible.

This will rarely result in `capacity() == len_bytes()`. `Rope` stores text in a sequence of fixed-capacity chunks, so an exact fit only happens for texts that are both a precise multiple of that capacity *and* have code point boundaries that line up exactly with the capacity boundaries.

After calling this, the difference between `capacity()` and `len_bytes()` is typically under 1KB per megabyte of text in the `Rope`.

**NOTE:** calling this on a `Rope` clone causes it to stop sharing all data with its other clones. In such cases you will very likely be *increasing* total memory usage despite shrinking the `Rope`’s capacity.

Runs in O(N) time, and uses O(log N) additional space during shrinking.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#343-346)

#### pub fn insert(&mut self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), text: &[str](https://doc.rust-lang.org/nightly/std/primitive.str.html))

Inserts `text` at char index `char_idx`.

Runs in O(M + log N) time, where N is the length of the `Rope` and M is the length of `text`.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#356-358)

#### pub fn insert_char(&mut self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ch: [char](https://doc.rust-lang.org/nightly/std/primitive.char.html))

Inserts a single char `ch` at char index `char_idx`.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#542-547)

#### pub fn remove<R>(&mut self, char_range: R) where R: [RangeBounds](https://doc.rust-lang.org/nightly/core/ops/range/trait.RangeBounds.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>,

Removes the text in the given char index range.

Uses range syntax, e.g. `2..7`, `2..`, etc. The range is in `char` indices.

Runs in O(M + log N) time, where N is the length of the `Rope` and M is the length of the range being removed.

##### Example

```rust
let mut rope = Rope::from_str("Hello world!");
rope.remove(5..);

assert_eq!("Hello", rope);
```

##### Panics

Panics if the start of the range is greater than the end, or if the end is out of bounds (i.e. `end > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#557-559)

#### pub fn split_off(&mut self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> Self

Splits the `Rope` at `char_idx`, returning the right part of the split.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#564-614)

#### pub fn append(&mut self, other: Self)

Appends a `Rope` to the end of this one, consuming the other `Rope`.

Runs in O(log N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#634-636)

#### pub fn byte_to_char(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the char index of the given byte.

Notes:

- If the byte is in the middle of a multi-byte char, returns the index of the char that the byte belongs to.
- `byte_idx` can be one-past-the-end, which will return one-past-the-end char index.

Runs in O(log N) time.

##### Panics

Panics if `byte_idx` is out of bounds (i.e. `byte_idx > len_bytes()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#653-655)

#### pub fn byte_to_line(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the line index of the given byte.

Notes:

- Lines are zero-indexed. This is functionally equivalent to counting the line endings before the specified byte.
- `byte_idx` can be one-past-the-end, which will return the last line index.

Runs in O(log N) time.

##### Panics

Panics if `byte_idx` is out of bounds (i.e. `byte_idx > len_bytes()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#670-672)

#### pub fn char_to_byte(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the byte index of the given char.

Notes:

- `char_idx` can be one-past-the-end, which will return one-past-the-end byte index.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#689-691)

#### pub fn char_to_line(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the line index of the given char.

Notes:

- Lines are zero-indexed. This is functionally equivalent to counting the line endings before the specified char.
- `char_idx` can be one-past-the-end, which will return the last line index.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#706-708)

#### pub fn char_to_utf16_cu(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the utf16 code unit index of the given char.

Ropey stores text internally as utf8, but sometimes it is necessary to interact with external APIs that still use utf16. This function is primarily intended for such situations, and is otherwise not very useful.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#727-729)

#### pub fn utf16_cu_to_char(&self, utf16_cu_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the char index of the given utf16 code unit.

Ropey stores text internally as utf8, but sometimes it is necessary to interact with external APIs that still use utf16. This function is primarily intended for such situations, and is otherwise not very useful.

Note: if the utf16 code unit is in the middle of a char, returns the index of the char that it belongs to.

Runs in O(log N) time.

##### Panics

Panics if `utf16_cu_idx` is out of bounds (i.e. `utf16_cu_idx > len_utf16_cu()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#745-747)

#### pub fn line_to_byte(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the byte index of the start of the given line.

Notes:

- Lines are zero-indexed.
- `line_idx` can be one-past-the-end, which will return one-past-the-end byte index.

Runs in O(log N) time.

##### Panics

Panics if `line_idx` is out of bounds (i.e. `line_idx > len_lines()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#763-765)

#### pub fn line_to_char(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)

Returns the char index of the start of the given line.

Notes:

- Lines are zero-indexed.
- `line_idx` can be one-past-the-end, which will return one-past-the-end char index.

Runs in O(log N) time.

##### Panics

Panics if `line_idx` is out of bounds (i.e. `line_idx > len_lines()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#778-789)

#### pub fn byte(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [u8](https://doc.rust-lang.org/nightly/std/primitive.u8.html)

Returns the byte at `byte_idx`.

Runs in O(log N) time.

##### Panics

Panics if `byte_idx` is out of bounds (i.e. `byte_idx >= len_bytes()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#799-809)

#### pub fn char(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [char](https://doc.rust-lang.org/nightly/std/primitive.char.html)

Returns the char at `char_idx`.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx >= len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#821-831)

#### pub fn line(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'_>

Returns the line at `line_idx`.

Note: lines are zero-indexed.

Runs in O(log N) time.

##### Panics

Panics if `line_idx` is out of bounds (i.e. `line_idx >= len_lines()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#850-861)

#### pub fn chunk_at_byte(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> (&[str](https://doc.rust-lang.org/nightly/std/primitive.str.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))

Returns the chunk containing the given byte index.

Also returns the byte and char indices of the beginning of the chunk and the index of the line that the chunk starts on.

Note: for convenience, a one-past-the-end `byte_idx` returns the last chunk of the `RopeSlice`.

The return value is organized as `(chunk, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.

Runs in O(log N) time.

##### Panics

Panics if `byte_idx` is out of bounds (i.e. `byte_idx > len_bytes()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#880-890)

#### pub fn chunk_at_char(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> (&[str](https://doc.rust-lang.org/nightly/std/primitive.str.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))

Returns the chunk containing the given char index.

Also returns the byte and char indices of the beginning of the chunk and the index of the line that the chunk starts on.

Note: for convenience, a one-past-the-end `char_idx` returns the last chunk of the `RopeSlice`.

The return value is organized as `(chunk, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#912-922)

#### pub fn chunk_at_line_break( &self, line_break_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> (&[str](https://doc.rust-lang.org/nightly/std/primitive.str.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))

Returns the chunk containing the given line break.

Also returns the byte and char indices of the beginning of the chunk and the index of the line that the chunk starts on.

Note: for convenience, both the beginning and end of the rope are considered line breaks for the purposes of indexing. For example, in the string `"Hello \n world!"` 0 would give the first chunk, 1 would give the chunk containing the newline character, and 2 would give the last chunk.

The return value is organized as `(chunk, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.

Runs in O(log N) time.

##### Panics

Panics if `line_break_idx` is out of bounds (i.e. `line_break_idx > len_lines()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#948-953)

#### pub fn slice<R>(&self, char_range: R) -> [RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'_> where R: [RangeBounds](https://doc.rust-lang.org/nightly/core/ops/range/trait.RangeBounds.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>,

Gets an immutable slice of the `Rope`, using char indices.

Uses range syntax, e.g. `2..7`, `2..`, etc.

##### Example

```rust
let rope = Rope::from_str("Hello world!");
let slice = rope.slice(..5);

assert_eq!("Hello", slice);
```

Runs in O(log N) time.

##### Panics

Panics if the start of the range is greater than the end, or if the end is out of bounds (i.e. `end > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#967-975)

#### pub fn byte_slice<R>(&self, byte_range: R) -> [RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'_> where R: [RangeBounds](https://doc.rust-lang.org/nightly/core/ops/range/trait.RangeBounds.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>,

Gets and immutable slice of the `Rope`, using byte indices.

Uses range syntax, e.g. `2..7`, `2..`, etc.

Runs in O(log N) time.

##### Panics

Panics if:

- The start of the range is greater than the end.
- The end is out of bounds (i.e. `end > len_bytes()`).
- The range doesn’t align with char boundaries.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#984-986)

#### pub fn bytes(&self) -> [Bytes](https://docs.rs/ropey/latest/ropey/iter/struct.Bytes.html)<'_>

Creates an iterator over the bytes of the `Rope`.

Runs in O(log N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1000-1010)

#### pub fn bytes_at(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Bytes](https://docs.rs/ropey/latest/ropey/iter/struct.Bytes.html)<'_>

Creates an iterator over the bytes of the `Rope`, starting at byte `byte_idx`.

If `byte_idx == len_bytes()` then an iterator at the end of the `Rope` is created (i.e. `next()` will return `None`).

Runs in O(log N) time.

##### Panics

Panics if `byte_idx` is out of bounds (i.e. `byte_idx > len_bytes()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1016-1018)

#### pub fn chars(&self) -> [Chars](https://docs.rs/ropey/latest/ropey/iter/struct.Chars.html)<'_>

Creates an iterator over the chars of the `Rope`.

Runs in O(log N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1032-1042)

#### pub fn chars_at(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Chars](https://docs.rs/ropey/latest/ropey/iter/struct.Chars.html)<'_>

Creates an iterator over the chars of the `Rope`, starting at char `char_idx`.

If `char_idx == len_chars()` then an iterator at the end of the `Rope` is created (i.e. `next()` will return `None`).

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1048-1050)

#### pub fn lines(&self) -> [Lines](https://docs.rs/ropey/latest/ropey/iter/struct.Lines.html)<'_>

Creates an iterator over the lines of the `Rope`.

Runs in O(log N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1064-1074)

#### pub fn lines_at(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Lines](https://docs.rs/ropey/latest/ropey/iter/struct.Lines.html)<'_>

Creates an iterator over the lines of the `Rope`, starting at line `line_idx`.

If `line_idx == len_lines()` then an iterator at the end of the `Rope` is created (i.e. `next()` will return `None`).

Runs in O(log N) time.

##### Panics

Panics if `line_idx` is out of bounds (i.e. `line_idx > len_lines()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1080-1082)

#### pub fn chunks(&self) -> [Chunks](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html)<'_>

Creates an iterator over the chunks of the `Rope`.

Runs in O(log N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1102-1112)

#### pub fn chunks_at_byte( &self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> ([Chunks](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html)<'_>, [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))

Creates an iterator over the chunks of the `Rope`, with the iterator starting at the chunk containing `byte_idx`.

Also returns the byte and char indices of the beginning of the first chunk to be yielded, and the index of the line that chunk starts on.

If `byte_idx == len_bytes()` an iterator at the end of the `Rope` (yielding `None` on a call to `next()`) is created.

The return value is organized as `(iterator, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.

Runs in O(log N) time.

##### Panics

Panics if `byte_idx` is out of bounds (i.e. `byte_idx > len_bytes()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1132-1142)

#### pub fn chunks_at_char( &self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> ([Chunks](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html)<'_>, [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))

Creates an iterator over the chunks of the `Rope`, with the iterator starting at the chunk containing `char_idx`.

Also returns the byte and char indices of the beginning of the first chunk to be yielded, and the index of the line that chunk starts on.

If `char_idx == len_chars()` an iterator at the end of the `Rope` (yielding `None` on a call to `next()`) is created.

The return value is organized as `(iterator, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.

Runs in O(log N) time.

##### Panics

Panics if `char_idx` is out of bounds (i.e. `char_idx > len_chars()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1166-1176)

#### pub fn chunks_at_line_break( &self, line_break_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> ([Chunks](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html)<'_>, [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))

Creates an iterator over the chunks of the `Rope`, with the iterator starting at the chunk containing `line_break_idx`.

Also returns the byte and char indices of the beginning of the first chunk to be yielded, and the index of the line that chunk starts on.

Note: for convenience, both the beginning and end of the `Rope` are considered line breaks for the purposes of indexing. For example, in the string `"Hello \n world!"` 0 would create an iterator starting on the first chunk, 1 would create an iterator starting on the chunk containing the newline character, and 2 would create an iterator at the end of the `Rope` (yielding `None` on a call to `next()`).

The return value is organized as `(iterator, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.

Runs in O(log N) time.

##### Panics

Panics if `line_break_idx` is out of bounds (i.e. `line_break_idx > len_lines()`).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1195-1197)

#### pub fn is_instance(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Returns true if this rope and `other` point to precisely the same in-memory data.

This happens when one of the ropes is a clone of the other and neither have been modified since then. Because clones initially share all the same data, it can be useful to check if they still point to precisely the same memory as a way of determining whether they are both still unmodified.

Note: this is distinct from checking for equality: two ropes can have the same *contents* (equal) but be stored in different memory locations (not instances). Importantly, two clones that post-cloning are modified identically will *not* be instances anymore, even though they will have equal contents.

Runs in O(1) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1266-1822)

### impl [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

#### Non-Panicking

The methods in this impl block provide non-panicking versions of `Rope`’s panicking methods. They return either `Option::None` or `Result::Err()` when their panicking counterparts would have panicked.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1269-1320)

#### pub fn try_insert(&mut self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), text: &[str](https://doc.rust-lang.org/nightly/std/primitive.str.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[()](https://doc.rust-lang.org/nightly/std/primitive.unit.html)>

Non-panicking version of [`insert()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.insert).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1324-1333)

#### pub fn try_insert_char(&mut self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ch: [char](https://doc.rust-lang.org/nightly/std/primitive.char.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[()](https://doc.rust-lang.org/nightly/std/primitive.unit.html)>

Non-panicking version of [`insert_char()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.insert_char).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1336-1377)

#### pub fn try_remove<R>(&mut self, char_range: R) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[()](https://doc.rust-lang.org/nightly/std/primitive.unit.html)> where R: [RangeBounds](https://doc.rust-lang.org/nightly/core/ops/range/trait.RangeBounds.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>,

Non-panicking version of [`remove()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.remove).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1380-1408)

#### pub fn try_split_off(&mut self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<Self>

Non-panicking version of [`split_off()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.split_off).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1412-1420)

#### pub fn try_byte_to_char(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`byte_to_char()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.byte_to_char).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1424-1432)

#### pub fn try_byte_to_line(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`byte_to_line()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.byte_to_line).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1436-1444)

#### pub fn try_char_to_byte(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`char_to_byte()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.char_to_byte).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1448-1456)

#### pub fn try_char_to_line(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`char_to_line()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.char_to_line).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1460-1472)

#### pub fn try_char_to_utf16_cu(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`char_to_utf16_cu()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.char_to_utf16_cu).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1476-1491)

#### pub fn try_utf16_cu_to_char(&self, utf16_cu_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`utf16_cu_to_char()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.utf16_cu_to_char).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1495-1507)

#### pub fn try_line_to_byte(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`line_to_byte()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.line_to_byte).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1511-1523)

#### pub fn try_line_to_char(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Result](https://docs.rs/ropey/latest/ropey/type.Result.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>

Non-panicking version of [`line_to_char()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.line_to_char).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1527-1536)

#### pub fn get_byte(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[u8](https://doc.rust-lang.org/nightly/std/primitive.u8.html)>

Non-panicking version of [`byte()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.byte).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1540-1549)

#### pub fn get_char(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[char](https://doc.rust-lang.org/nightly/std/primitive.char.html)>

Non-panicking version of [`char()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.char).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1553-1580)

#### pub fn get_line(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'_>>

Non-panicking version of [`line()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.line).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1584-1597)

#### pub fn get_chunk_at_byte( &self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<(&[str](https://doc.rust-lang.org/nightly/std/primitive.str.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))>

Non-panicking version of [`chunk_at_byte()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.chunk_at_byte).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1601-1614)

#### pub fn get_chunk_at_char( &self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<(&[str](https://doc.rust-lang.org/nightly/std/primitive.str.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))>

Non-panicking version of [`chunk_at_char()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.chunk_at_char).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1618-1634)

#### pub fn get_chunk_at_line_break( &self, line_break_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<(&[str](https://doc.rust-lang.org/nightly/std/primitive.str.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))>

Non-panicking version of [`chunk_at_line_break()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.chunk_at_line_break).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1638-1651)

#### pub fn get_slice<R>(&self, char_range: R) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'_>> where R: [RangeBounds](https://doc.rust-lang.org/nightly/core/ops/range/trait.RangeBounds.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>,

Non-panicking version of [`slice()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.slice).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1655-1660)

#### pub fn get_byte_slice<R>(&self, byte_range: R) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'_>> where R: [RangeBounds](https://doc.rust-lang.org/nightly/core/ops/range/trait.RangeBounds.html)<[usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)>,

Non-panicking version of [`byte_slice()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.byte_slice).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1719-1733)

#### pub fn get_bytes_at(&self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[Bytes](https://docs.rs/ropey/latest/ropey/iter/struct.Bytes.html)<'_>>

Non-panicking version of [`bytes_at()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.bytes_at).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1737-1751)

#### pub fn get_chars_at(&self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[Chars](https://docs.rs/ropey/latest/ropey/iter/struct.Chars.html)<'_>>

Non-panicking version of [`chars_at()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.chars_at).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1755-1767)

#### pub fn get_lines_at(&self, line_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html)) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[Lines](https://docs.rs/ropey/latest/ropey/iter/struct.Lines.html)<'_>>

Non-panicking version of [`lines_at()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.lines_at).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1771-1784)

#### pub fn get_chunks_at_byte( &self, byte_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<([Chunks](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html)<'_>, [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))>

Non-panicking version of [`chunks_at_byte()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.chunks_at_byte).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1788-1801)

#### pub fn get_chunks_at_char( &self, char_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<([Chunks](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html)<'_>, [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))>

Non-panicking version of [`chunks_at_char()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.chunks_at_char).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1805-1821)

#### pub fn get_chunks_at_line_break( &self, line_break_idx: [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), ) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<([Chunks](https://docs.rs/ropey/latest/ropey/iter/struct.Chunks.html)<'_>, [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html), [usize](https://doc.rust-lang.org/nightly/std/primitive.usize.html))>

Non-panicking version of [`chunks_at_line_break()`](https://docs.rs/ropey/latest/ropey/struct.Rope.html#method.chunks_at_line_break).

## Trait Implementations

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#81)

### impl [Clone](https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#81)

#### fn [clone](https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html#tymethod.clone)(&self) -> Self

Returns a duplicate of the value. [Read more](https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html#tymethod.clone)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/142757)) · [Source](https://doc.rust-lang.org/nightly/src/core/clone.rs.html#245-247)

#### fn [clone_from](https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html#method.clone_from)(&mut self, source: &Self)

Performs copy-assignment from `source`. [Read more](https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html#method.clone_from)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1972-1976)

### impl [Debug](https://doc.rust-lang.org/nightly/core/fmt/trait.Debug.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1973-1975)

#### fn [fmt](https://doc.rust-lang.org/nightly/core/fmt/trait.Debug.html#tymethod.fmt)(&self, f: &mut [Formatter](https://doc.rust-lang.org/nightly/core/fmt/struct.Formatter.html)<'_>) -> [Result](https://doc.rust-lang.org/nightly/core/fmt/type.Result.html)

Formats the value using the given formatter. [Read more](https://doc.rust-lang.org/nightly/core/fmt/trait.Debug.html#tymethod.fmt)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1988-1993)

### impl [Default](https://doc.rust-lang.org/nightly/core/default/trait.Default.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1990-1992)

#### fn [default](https://doc.rust-lang.org/nightly/core/default/trait.Default.html#tymethod.default)() -> Self

Returns the “default value” for a type. [Read more](https://doc.rust-lang.org/nightly/core/default/trait.Default.html#tymethod.default)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1978-1986)

### impl [Display](https://doc.rust-lang.org/nightly/core/fmt/trait.Display.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1980-1985)

#### fn [fmt](https://doc.rust-lang.org/nightly/core/fmt/trait.Display.html#tymethod.fmt)(&self, f: &mut [Formatter](https://doc.rust-lang.org/nightly/core/fmt/struct.Formatter.html)<'_>) -> [Result](https://doc.rust-lang.org/nightly/core/fmt/type.Result.html)

Formats the value using the given formatter. [Read more](https://doc.rust-lang.org/nightly/core/fmt/trait.Display.html#tymethod.fmt)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1995)

### impl [Eq](https://doc.rust-lang.org/nightly/core/cmp/trait.Eq.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1899-1906)

### impl<'a> [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<&'a [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1901-1905)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(r: &'a [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1919-1928)

### impl<'a> [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<&'a [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>

Attempts to borrow the contents of the `Rope`, but will convert to an owned string if the contents is not contiguous in memory.

Runs in best case O(1), worst case O(N).

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1921-1927)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(r: &'a [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1827-1832)

### impl<'a> [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<&'a [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1829-1831)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(text: &'a [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1834-1839)

### impl<'a> [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<[Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1836-1838)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(text: [Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1892-1897)

### impl [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1894-1896)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(r: [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1908-1913)

### impl<'a> [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1910-1912)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(r: [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1851-1890)

### impl<'a> [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<[RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'a>> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

Will share data where possible.

Runs in O(log N) time.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1852-1889)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(s: [RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'a>) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1841-1846)

### impl [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<[String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1843-1845)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(text: [String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)) -> Self

Converts to this type from the input type.

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1930-1941)

### impl<'a> [FromIterator](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html)<&'a [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1931-1940)

#### fn [from_iter](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html#tymethod.from_iter)<T>(iter: T) -> Self where T: [IntoIterator](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.IntoIterator.html)<Item = &'a [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>,

Creates a value from an iterator. [Read more](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html#tymethod.from_iter)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1943-1954)

### impl<'a> [FromIterator](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html)<[Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1944-1953)

#### fn [from_iter](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html#tymethod.from_iter)<T>(iter: T) -> Self where T: [IntoIterator](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.IntoIterator.html)<Item = [Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>>,

Creates a value from an iterator. [Read more](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html#tymethod.from_iter)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1956-1967)

### impl [FromIterator](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html)<[String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1957-1966)

#### fn [from_iter](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html#tymethod.from_iter)<T>(iter: T) -> Self where T: [IntoIterator](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.IntoIterator.html)<Item = [String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)>,

Creates a value from an iterator. [Read more](https://doc.rust-lang.org/nightly/core/iter/traits/collect/trait.FromIterator.html#tymethod.from_iter)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2074-2078)

### impl [Hash](https://doc.rust-lang.org/nightly/core/hash/trait.Hash.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2075-2077)

#### fn [hash](https://doc.rust-lang.org/nightly/core/hash/trait.Hash.html#tymethod.hash)<H: [Hasher](https://doc.rust-lang.org/nightly/core/hash/trait.Hasher.html)>(&self, state: [&mut H](https://doc.rust-lang.org/nightly/std/primitive.reference.html))

Feeds this value into the given [`Hasher`](https://doc.rust-lang.org/nightly/core/hash/trait.Hasher.html). [Read more](https://doc.rust-lang.org/nightly/core/hash/trait.Hash.html#tymethod.hash)

1.3.0 · [Source](https://doc.rust-lang.org/nightly/src/core/hash/mod.rs.html#234-236)

#### fn [hash_slice](https://doc.rust-lang.org/nightly/core/hash/trait.Hash.html#method.hash_slice)<H>(data: &[Self], state: [&mut H](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) where H: [Hasher](https://doc.rust-lang.org/nightly/core/hash/trait.Hasher.html), Self: [Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

Feeds a slice of this type into the given [`Hasher`](https://doc.rust-lang.org/nightly/core/hash/trait.Hasher.html). [Read more](https://doc.rust-lang.org/nightly/core/hash/trait.Hash.html#method.hash_slice)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2060-2065)

### impl [Ord](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2062-2064)

#### fn [cmp](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#tymethod.cmp)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [Ordering](https://doc.rust-lang.org/nightly/core/cmp/enum.Ordering.html)

This method returns an [`Ordering`](https://doc.rust-lang.org/nightly/core/cmp/enum.Ordering.html) between `self` and `other`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#tymethod.cmp)

1.21.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1097-1099)

#### fn [max](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.max)(self, other: Self) -> Self where Self: [Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

Compares and returns the maximum of two values. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.max)

1.21.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1136-1138)

#### fn [min](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.min)(self, other: Self) -> Self where Self: [Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

Compares and returns the minimum of two values. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.min)

1.50.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1162-1164)

#### fn [clamp](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.clamp)(self, min: Self, max: Self) -> Self where Self: [Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

Restrict a value to a certain interval. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.clamp)

[Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1197-1200)

#### fn [clamp_to](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.clamp_to)<R>(self, range: R) -> Self where Self: [Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html), R: [ClampBounds](https://doc.rust-lang.org/nightly/core/cmp/clamp/trait.ClampBounds.html)<Self>,

🔬This is a nightly-only experimental API. (`clamp_to`)
Restrict a value to a certain range. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.Ord.html#method.clamp_to)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1997-2002)

### impl [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#1999-2001)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2004-2009)

### impl<'a> [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<&'a [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2006-2008)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &&'a [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2046-2051)

### impl<'a> [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2048-2050)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2011-2016)

### impl<'a> [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for &'a [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2013-2015)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2025-2030)

### impl [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2027-2029)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2039-2044)

### impl [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2041-2043)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2053-2058)

### impl<'a> [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [Cow](https://doc.rust-lang.org/nightly/alloc/borrow/enum.Cow.html)<'a, [str](https://doc.rust-lang.org/nightly/std/primitive.str.html)>

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2055-2057)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/slice.rs.html#1918-1923)

### impl<'a> [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)> for [RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'a>

[Source](https://docs.rs/ropey/latest/src/ropey/slice.rs.html#1920-1922)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/slice.rs.html#1925-1930)

### impl<'a> [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'a>> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/slice.rs.html#1927-1929)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[RopeSlice](https://docs.rs/ropey/latest/ropey/struct.RopeSlice.html)<'a>) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2032-2037)

### impl [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2034-2036)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2018-2023)

### impl [PartialEq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html)<[str](https://doc.rust-lang.org/nightly/std/primitive.str.html)> for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2020-2022)

#### fn [eq](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)(&self, other: &[str](https://doc.rust-lang.org/nightly/std/primitive.str.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Equality operator `==`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#tymethod.eq)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#278)

#### fn [ne](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Inequality operator `!=`. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialEq.html#method.ne)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2067-2072)

### impl [PartialOrd](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

[Source](https://docs.rs/ropey/latest/src/ropey/rope.rs.html#2069-2071)

#### fn [partial_cmp](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#tymethod.partial_cmp)(&self, other: &[Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)) -> [Option](https://doc.rust-lang.org/nightly/core/option/enum.Option.html)<[Ordering](https://doc.rust-lang.org/nightly/core/cmp/enum.Ordering.html)>

This method returns an ordering between `self` and `other` values if one exists. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#tymethod.partial_cmp)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1502)

#### fn [lt](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.lt)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Tests less than (for `self` and `other`) and is used by the `<` operator. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.lt)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1520)

#### fn [le](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.le)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Tests less than or equal to (for `self` and `other`) and is used by the `<=` operator. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.le)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1538)

#### fn [gt](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.gt)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Tests greater than (for `self` and `other`) and is used by the `>` operator. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.gt)

1.0.0 (const: [unstable](https://github.com/rust-lang/rust/issues/143800)) · [Source](https://doc.rust-lang.org/nightly/src/core/cmp.rs.html#1556)

#### fn [ge](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.ge)(&self, other: [&Rhs](https://doc.rust-lang.org/nightly/std/primitive.reference.html)) -> [bool](https://doc.rust-lang.org/nightly/std/primitive.bool.html)

Tests greater than or equal to (for `self` and `other`) and is used by the `>=` operator. [Read more](https://doc.rust-lang.org/nightly/core/cmp/trait.PartialOrd.html#method.ge)

## Auto Trait Implementations

### impl [Freeze](https://doc.rust-lang.org/nightly/core/marker/trait.Freeze.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

### impl [RefUnwindSafe](https://doc.rust-lang.org/nightly/core/panic/unwind_safe/trait.RefUnwindSafe.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

### impl [Send](https://doc.rust-lang.org/nightly/core/marker/trait.Send.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

### impl [Sync](https://doc.rust-lang.org/nightly/core/marker/trait.Sync.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

### impl [Unpin](https://doc.rust-lang.org/nightly/core/marker/trait.Unpin.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

### impl [UnsafeUnpin](https://doc.rust-lang.org/nightly/core/marker/trait.UnsafeUnpin.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

### impl [UnwindSafe](https://doc.rust-lang.org/nightly/core/panic/unwind_safe/trait.UnwindSafe.html) for [Rope](https://docs.rs/ropey/latest/ropey/struct.Rope.html)

## Blanket Implementations

[Source](https://doc.rust-lang.org/nightly/src/core/any.rs.html#142)

### impl<T> [Any](https://doc.rust-lang.org/nightly/core/any/trait.Any.html) for T where T: 'static + ?[Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

[Source](https://doc.rust-lang.org/nightly/src/core/any.rs.html#143)

#### fn [type_id](https://doc.rust-lang.org/nightly/core/any/trait.Any.html#tymethod.type_id)(&self) -> [TypeId](https://doc.rust-lang.org/nightly/core/any/struct.TypeId.html)

Gets the `TypeId` of `self`. [Read more](https://doc.rust-lang.org/nightly/core/any/trait.Any.html#tymethod.type_id)

[Source](https://doc.rust-lang.org/nightly/src/core/borrow.rs.html#212)

### impl<T> [Borrow](https://doc.rust-lang.org/nightly/core/borrow/trait.Borrow.html)<T> for T where T: ?[Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

[Source](https://doc.rust-lang.org/nightly/src/core/borrow.rs.html#214)

#### fn [borrow](https://doc.rust-lang.org/nightly/core/borrow/trait.Borrow.html#tymethod.borrow)(&self) -> [&T](https://doc.rust-lang.org/nightly/std/primitive.reference.html)

Immutably borrows from an owned value. [Read more](https://doc.rust-lang.org/nightly/core/borrow/trait.Borrow.html#tymethod.borrow)

[Source](https://doc.rust-lang.org/nightly/src/core/borrow.rs.html#221)

### impl<T> [BorrowMut](https://doc.rust-lang.org/nightly/core/borrow/trait.BorrowMut.html)<T> for T where T: ?[Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

[Source](https://doc.rust-lang.org/nightly/src/core/borrow.rs.html#222)

#### fn [borrow_mut](https://doc.rust-lang.org/nightly/core/borrow/trait.BorrowMut.html#tymethod.borrow_mut)(&mut self) -> [&mut T](https://doc.rust-lang.org/nightly/std/primitive.reference.html)

Mutably borrows from an owned value. [Read more](https://doc.rust-lang.org/nightly/core/borrow/trait.BorrowMut.html#tymethod.borrow_mut)

[Source](https://doc.rust-lang.org/nightly/src/core/clone.rs.html#648)

### impl<T> [CloneToUninit](https://doc.rust-lang.org/nightly/core/clone/trait.CloneToUninit.html) for T where T: [Clone](https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html),

[Source](https://doc.rust-lang.org/nightly/src/core/clone.rs.html#650)

#### unsafe fn [clone_to_uninit](https://doc.rust-lang.org/nightly/core/clone/trait.CloneToUninit.html#tymethod.clone_to_uninit)(&self, dest: [*mut](https://doc.rust-lang.org/nightly/std/primitive.pointer.html) [u8](https://doc.rust-lang.org/nightly/std/primitive.u8.html))

🔬This is a nightly-only experimental API. (`clone_to_uninit`)
Performs copy-assignment from `self` to `dest`. [Read more](https://doc.rust-lang.org/nightly/core/clone/trait.CloneToUninit.html#tymethod.clone_to_uninit)

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#787)

### impl<T> [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<T> for T

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#790)

#### fn [from](https://doc.rust-lang.org/nightly/core/convert/trait.From.html#tymethod.from)(t: T) -> T

Returns the argument unchanged.

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#769-771)

### impl<T, U> [Into](https://doc.rust-lang.org/nightly/core/convert/trait.Into.html)<U> for T where U: [From](https://doc.rust-lang.org/nightly/core/convert/trait.From.html)<T>,

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#779)

#### fn [into](https://doc.rust-lang.org/nightly/core/convert/trait.Into.html#tymethod.into)(self) -> U

Calls `U::from(self)`.

That is, this conversion is whatever the implementation of `From<T> for U` chooses to do.

[Source](https://doc.rust-lang.org/nightly/src/alloc/borrow.rs.html#72-74)

### impl<T> [ToOwned](https://doc.rust-lang.org/nightly/alloc/borrow/trait.ToOwned.html) for T where T: [Clone](https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html),

[Source](https://doc.rust-lang.org/nightly/src/alloc/borrow.rs.html#76)

#### type [Owned](https://doc.rust-lang.org/nightly/alloc/borrow/trait.ToOwned.html#associatedtype.Owned) = T

The resulting type after obtaining ownership.

[Source](https://doc.rust-lang.org/nightly/src/alloc/borrow.rs.html#77)

#### fn [to_owned](https://doc.rust-lang.org/nightly/alloc/borrow/trait.ToOwned.html#tymethod.to_owned)(&self) -> T

Creates owned data from borrowed data, usually by cloning. [Read more](https://doc.rust-lang.org/nightly/alloc/borrow/trait.ToOwned.html#tymethod.to_owned)

[Source](https://doc.rust-lang.org/nightly/src/alloc/borrow.rs.html#81)

#### fn [clone_into](https://doc.rust-lang.org/nightly/alloc/borrow/trait.ToOwned.html#method.clone_into)(&self, target: [&mut T](https://doc.rust-lang.org/nightly/std/primitive.reference.html))

Uses borrowed data to replace owned data, usually by cloning. [Read more](https://doc.rust-lang.org/nightly/alloc/borrow/trait.ToOwned.html#method.clone_into)

[Source](https://doc.rust-lang.org/nightly/src/alloc/string.rs.html#2972)

### impl<T> [ToString](https://doc.rust-lang.org/nightly/alloc/string/trait.ToString.html) for T where T: [Display](https://doc.rust-lang.org/nightly/core/fmt/trait.Display.html) + ?[Sized](https://doc.rust-lang.org/nightly/core/marker/trait.Sized.html),

[Source](https://doc.rust-lang.org/nightly/src/alloc/string.rs.html#2974)

#### fn [to_string](https://doc.rust-lang.org/nightly/alloc/string/trait.ToString.html#tymethod.to_string)(&self) -> [String](https://doc.rust-lang.org/nightly/alloc/string/struct.String.html)

Converts the given value to a `String`. [Read more](https://doc.rust-lang.org/nightly/alloc/string/trait.ToString.html#tymethod.to_string)

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#814-816)

### impl<T, U> [TryFrom](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html)<U> for T where U: [Into](https://doc.rust-lang.org/nightly/core/convert/trait.Into.html)<T>,

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#818)

#### type [Error](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html#associatedtype.Error) = [!](https://doc.rust-lang.org/nightly/std/primitive.never.html)

The type returned in the event of a conversion error.

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#821)

#### fn [try_from](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html#tymethod.try_from)(value: U) -> [Result](https://doc.rust-lang.org/nightly/core/result/enum.Result.html)<T, [!](https://doc.rust-lang.org/nightly/std/primitive.never.html)>

Performs the conversion.

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#798-800)

### impl<T, U> [TryInto](https://doc.rust-lang.org/nightly/core/convert/trait.TryInto.html)<U> for T where U: [TryFrom](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html)<T>,

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#802)

#### type [Error](https://doc.rust-lang.org/nightly/core/convert/trait.TryInto.html#associatedtype.Error) = <U as [TryFrom](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html)<T>>::[Error](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html#associatedtype.Error)

The type returned in the event of a conversion error.

[Source](https://doc.rust-lang.org/nightly/src/core/convert/mod.rs.html#805)

#### fn [try_into](https://doc.rust-lang.org/nightly/core/convert/trait.TryInto.html#tymethod.try_into)(self) -> [Result](https://doc.rust-lang.org/nightly/core/result/enum.Result.html)<U, <U as [TryFrom](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html)<T>>::[Error](https://doc.rust-lang.org/nightly/core/convert/trait.TryFrom.html#associatedtype.Error)>

Performs the conversion.

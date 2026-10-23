## Writing

| Unlink and rename of a name held in an extended reference (`INODE_EXTREF`): the record leaves its item, and the item goes with its last record | Supported; a rename into a directory whose `INODE_REF` for the file is already full is refused, as extended references are not written | Unreleased (#424) | | `hard_link_extref.rs` |

## Interfaces

| `fs.btrfs xattr`: a path's extended attributes listed by full name and size, sorted, and one value's raw bytes (`--features cli`) | Supported | Unreleased (#416) | | `cli_xattr.rs`, `cli_xattr_kernel.rs`, `tests/cli/test-xattr.sh` |
| `fs.btrfs write --at`: a write at a position inside a file | Not supported (`not implemented`, exit 3) | Unreleased (#415) | | `cli_dispatch.rs`, `tests/cli/test-blocked.sh` |

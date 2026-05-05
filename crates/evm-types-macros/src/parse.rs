//! Parser for a Solidity-subset event declaration used by `event_signature!`.
//!
//! Accepted grammar (subset of Solidity):
//!
//! ```text
//! event <Name>(<param>, ...);
//!
//! param ::= <type> [indexed] <name>
//! type  ::= address | bool
//!         | uint<N>  (N ∈ {8,16,...,256})
//!         | int<N>
//!         | bytes<N> (N ∈ {1,...,32})
//!         | bytes
//!         | string
//! ```
//!
//! Deliberately minimal — adds new Solidity types as detectors need them.
//!
//! reference: alloy-sol-macro parser structure (MIT/Apache-2.0) — grammar
//!            approach consulted; this is a much smaller subset.

use syn::{
    Ident, LitStr, Token,
    parse::{Parse, ParseStream, Result as SynResult},
    punctuated::Punctuated,
};

// ---------------------------------------------------------------------------
// Public AST types
// ---------------------------------------------------------------------------

/// A parsed event declaration.
#[derive(Debug)]
pub struct EventDecl {
    /// The event name, e.g. `Transfer`.
    pub name: Ident,
    /// The parameter list.
    pub params: Vec<EventParam>,
}

/// A single event parameter.
#[derive(Debug)]
pub struct EventParam {
    /// The Solidity type, e.g. `address`, `uint256`.
    pub ty: SolType,
    /// Whether this parameter is indexed (stored in a topic).
    pub indexed: bool,
    /// The parameter name, e.g. `from`.
    pub name: Ident,
}

/// A Solidity type used in an event parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolType {
    /// `address`
    Address,
    /// `bool`
    Bool,
    /// `uint<N>` (N ∈ 8..=256, multiple of 8)
    Uint(u16),
    /// `int<N>` (N ∈ 8..=256, multiple of 8)
    Int(u16),
    /// `bytes<N>` (N ∈ 1..=32)
    BytesFixed(u8),
    /// `bytes` (dynamic)
    Bytes,
    /// `string` (dynamic)
    String,
}

impl SolType {
    /// Return the canonical ABI type string used in signature hashing.
    ///
    /// reference: https://docs.soliditylang.org/en/latest/abi-spec.html —
    ///   canonical type names are used for signature computation.
    pub fn canonical(&self) -> std::string::String {
        match self {
            SolType::Address => "address".into(),
            SolType::Bool => "bool".into(),
            SolType::Uint(n) => format!("uint{n}"),
            SolType::Int(n) => format!("int{n}"),
            SolType::BytesFixed(n) => format!("bytes{n}"),
            SolType::Bytes => "bytes".into(),
            SolType::String => "string".into(),
        }
    }

    /// Return `true` if this type is dynamic (head slot is an offset pointer).
    ///
    /// Used when building tuple-decode layouts with `resolve_field_offsets`.
    #[allow(dead_code)]
    pub fn is_dynamic(&self) -> bool {
        matches!(self, SolType::Bytes | SolType::String)
    }
}

// ---------------------------------------------------------------------------
// Parse implementation
// ---------------------------------------------------------------------------

impl Parse for EventDecl {
    fn parse(input: ParseStream<'_>) -> SynResult<Self> {
        // `event` keyword — we use a raw ident since `event` is not a Rust keyword.
        let kw: Ident = input.parse()?;
        if kw != "event" {
            return Err(syn::Error::new(kw.span(), "expected `event` keyword"));
        }

        let name: Ident = input.parse()?;

        // Parameter list in parentheses.
        let content;
        syn::parenthesized!(content in input);

        let raw_params: Punctuated<EventParam, Token![,]> =
            content.parse_terminated(EventParam::parse, Token![,])?;

        // Consume optional trailing `;` (Solidity style).
        let _ = input.parse::<Option<Token![;]>>();

        Ok(EventDecl { name, params: raw_params.into_iter().collect() })
    }
}

impl Parse for EventParam {
    fn parse(input: ParseStream<'_>) -> SynResult<Self> {
        let ty = parse_sol_type(input)?;

        // Optional `indexed` keyword.
        let indexed = if input.peek(Ident) {
            let peeked: Ident = input.fork().parse()?;
            if peeked == "indexed" {
                let _: Ident = input.parse()?;
                true
            } else {
                false
            }
        } else {
            false
        };

        let name: Ident = input.parse()?;

        Ok(EventParam { ty, indexed, name })
    }
}

/// Parse a Solidity type identifier from the token stream.
///
/// Handles bare identifiers like `address`, `bool`, `bytes`, `string`, and
/// typed identifiers like `uint256`, `int128`, `bytes32`.
fn parse_sol_type(input: ParseStream<'_>) -> SynResult<SolType> {
    // Types can be represented as:
    //   - a plain identifier: `address`, `bool`, `bytes`, `string`
    //   - an identifier followed by a number: represented as a single ident
    //     token in Rust's lexer (e.g. `uint256` is one token: `uint256`)
    //
    // `syn` lexes `uint256` as a single `Ident` token — the number suffix is
    // part of the identifier.  So we read one Ident and parse it as a string.

    // Try to parse as an Ident first; if that fails, try a LitStr (for
    // programmatic use, e.g. `event_signature!("Transfer(address,address,uint256)")`).
    if input.peek(LitStr) {
        // Accept a string literal representing the canonical signature — but
        // this path is not currently used by the macro; it's here for extensibility.
        let _: LitStr = input.parse()?;
        return Err(input.error("string literal event signature syntax not supported in this position"));
    }

    let ident: Ident = input.parse()?;
    let s = ident.to_string();

    if s == "address" {
        return Ok(SolType::Address);
    }
    if s == "bool" {
        return Ok(SolType::Bool);
    }
    if s == "bytes" {
        return Ok(SolType::Bytes);
    }
    if s == "string" {
        return Ok(SolType::String);
    }

    // uint<N>
    if let Some(rest) = s.strip_prefix("uint") {
        if rest.is_empty() {
            // bare `uint` == `uint256` per Solidity spec
            return Ok(SolType::Uint(256));
        }
        let n: u16 = rest.parse().map_err(|_| {
            syn::Error::new(ident.span(), format!("invalid uint bit-width: `{rest}`"))
        })?;
        if !(8..=256).contains(&n) || !n.is_multiple_of(8) {
            return Err(syn::Error::new(
                ident.span(),
                format!("uint bit-width must be 8..=256 and a multiple of 8, got {n}"),
            ));
        }
        return Ok(SolType::Uint(n));
    }

    // int<N>
    if let Some(rest) = s.strip_prefix("int") {
        if rest.is_empty() {
            return Ok(SolType::Int(256));
        }
        let n: u16 = rest.parse().map_err(|_| {
            syn::Error::new(ident.span(), format!("invalid int bit-width: `{rest}`"))
        })?;
        if !(8..=256).contains(&n) || !n.is_multiple_of(8) {
            return Err(syn::Error::new(
                ident.span(),
                format!("int bit-width must be 8..=256 and a multiple of 8, got {n}"),
            ));
        }
        return Ok(SolType::Int(n));
    }

    // bytes<N> — note: bare `bytes` was caught above
    if let Some(rest) = s.strip_prefix("bytes") {
        let n: u8 = rest.parse().map_err(|_| {
            syn::Error::new(ident.span(), format!("invalid bytesN size: `{rest}`"))
        })?;
        if n == 0 || n > 32 {
            return Err(syn::Error::new(
                ident.span(),
                format!("bytesN size must be 1..=32, got {n}"),
            ));
        }
        return Ok(SolType::BytesFixed(n));
    }

    Err(syn::Error::new(
        ident.span(),
        format!("unsupported Solidity type `{s}` — supported: address, bool, uint<N>, int<N>, bytes<N>, bytes, string"),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_str;

    #[test]
    fn parse_transfer() {
        let decl: EventDecl = parse_str(
            "event Transfer(address indexed from, address indexed to, uint256 value);",
        )
        .unwrap();
        assert_eq!(decl.name.to_string(), "Transfer");
        assert_eq!(decl.params.len(), 3);
        assert_eq!(decl.params[0].ty, SolType::Address);
        assert!(decl.params[0].indexed);
        assert_eq!(decl.params[0].name.to_string(), "from");
        assert_eq!(decl.params[1].ty, SolType::Address);
        assert!(decl.params[1].indexed);
        assert_eq!(decl.params[2].ty, SolType::Uint(256));
        assert!(!decl.params[2].indexed);
    }

    #[test]
    fn parse_univ2_swap() {
        let decl: EventDecl = parse_str(
            "event Swap(address indexed sender, uint256 amount0In, uint256 amount1In, \
             uint256 amount0Out, uint256 amount1Out, address indexed to);",
        )
        .unwrap();
        assert_eq!(decl.name.to_string(), "Swap");
        assert_eq!(decl.params.len(), 6);
        assert!(decl.params[0].indexed);
        assert!(!decl.params[1].indexed);
        assert_eq!(decl.params[5].ty, SolType::Address);
        assert!(decl.params[5].indexed);
    }

    #[test]
    fn parse_univ3_swap() {
        let decl: EventDecl = parse_str(
            "event Swap(address indexed sender, address indexed recipient, \
             int256 amount0, int256 amount1, uint160 sqrtPriceX96, \
             uint128 liquidity, int24 tick);",
        )
        .unwrap();
        assert_eq!(decl.params.len(), 7);
        assert_eq!(decl.params[2].ty, SolType::Int(256));
        assert_eq!(decl.params[4].ty, SolType::Uint(160));
        assert_eq!(decl.params[5].ty, SolType::Uint(128));
        assert_eq!(decl.params[6].ty, SolType::Int(24));
    }

    #[test]
    fn parse_bytes_dynamic() {
        let decl: EventDecl = parse_str(
            "event Foo(address indexed owner, bytes data);",
        )
        .unwrap();
        assert_eq!(decl.params[1].ty, SolType::Bytes);
        assert!(!decl.params[1].indexed);
    }

    #[test]
    fn parse_string_param() {
        let decl: EventDecl = parse_str(
            "event Foo(string name, bool flag);",
        )
        .unwrap();
        assert_eq!(decl.params[0].ty, SolType::String);
        assert_eq!(decl.params[1].ty, SolType::Bool);
    }

    #[test]
    fn parse_bytes32_param() {
        let decl: EventDecl = parse_str(
            "event HashStored(bytes32 indexed hash);",
        )
        .unwrap();
        assert_eq!(decl.params[0].ty, SolType::BytesFixed(32));
        assert!(decl.params[0].indexed);
    }

    #[test]
    fn canonical_types() {
        assert_eq!(SolType::Address.canonical(), "address");
        assert_eq!(SolType::Uint(256).canonical(), "uint256");
        assert_eq!(SolType::Int(24).canonical(), "int24");
        assert_eq!(SolType::BytesFixed(4).canonical(), "bytes4");
        assert_eq!(SolType::Bytes.canonical(), "bytes");
        assert_eq!(SolType::String.canonical(), "string");
        assert_eq!(SolType::Bool.canonical(), "bool");
    }
}

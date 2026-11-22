// Fixture for fts_bm25_ranking gate.
//
// Symbols defined here (hand-countable ground truth):
//
//   validate_all_incoming_user_supplied_request  — long snake_case name.
//       name-col tokens: "validate all incoming user supplied request" (6 tokens)
//       tokens-col:      "validate all incoming user supplied request"
//       The long name-col doc has poor BM25 length-norm on name col.
//       in_degree: 0 (no callers)
//
//   reValidate — short camelCase name.
//       name-col: "reValidate" (does NOT match "validate" as a name-col prefix)
//       tokens-col: "re validate" (short 2-token doc → strong BM25 stats)
//       in_degree: 0 (no callers)
//
// Filler symbols (no match for "validate"):
//   foo, bar, baz — restore IDF so scores are not all ~0.
//
// Under EQUAL weights (HEAD): reValidate wins (tokens-col short doc, better
//   length norm) → order: ["reValidate", "validate_all_incoming_user_supplied_request"]
// Under WEIGHTED bm25(10,1): name-col weight 10× flips it →
//   order: ["validate_all_incoming_user_supplied_request", "reValidate"]
//
// Both symbols have in_degree=0 so the tiebreak is inert.
// Alphabetically: reValidate < validate_all… so name-ASC tiebreak would give
//   ["reValidate", "validate_all…"] — the OPPOSITE of the weighted order,
//   proving the sort comes from bm25, not the tiebreak chain.

pub fn validate_all_incoming_user_supplied_request(data: &str) -> bool {
    !data.is_empty()
}

pub fn reValidate(data: &str) -> bool {
    data.len() > 0
}

pub fn foo() {}
pub fn bar() {}
pub fn baz() {}

// Shared between both codegen backends (native/Cranelift and WASM) —
// pure AST analysis, no backend-specific types, so it lives outside
// codegen.rs (which is gated behind the "native" feature and wouldn't
// compile for the wasm32 target kestrelc-web builds).

use crate::ast::*;
use crate::interner::Symbol;

// A recognized `where i < N` clause: `i` names a scalar parameter, `N`
// matches the symbolic size of some `[T; N]` parameter. `idx_pos`/
// `arr_pos` are that parameter's position in the *Kestrel* parameter
// list (not a backend's own ABI slot list — array params may take more
// than one ABI slot but always one Kestrel position), used to find the
// matching argument expression at a call site. Any other where-clause
// shape isn't recognized at all — no elision, no error, just the plain
// runtime check on every access, same as before this feature existed.
pub struct WhereInfo {
    pub idx_param: Symbol,
    pub arr_param: Symbol,
    pub idx_pos: usize,
    pub arr_pos: usize,
}

pub fn extract_where_info(f: &Fn) -> Option<WhereInfo> {
    let cond = f.where_clause.as_ref()?;
    let (left, right) = match cond {
        Expr::Binop { op: BinOp::Lt, left, right } => (left, right),
        _ => return None,
    };
    let idx_param = match left.as_ref() {
        Expr::Ident(n) => n.clone(),
        _ => return None,
    };
    let n_name = match right.as_ref() {
        Expr::Ident(n) => n.clone(),
        _ => return None,
    };
    let idx_pos = f.params.iter().position(|p| p.name == idx_param && matches!(p.ty, Type::Named(_)))?;
    let (arr_pos, arr_param) = f
        .params
        .iter()
        .enumerate()
        .find_map(|(i, p)| match &p.ty {
            Type::Array { size, .. } if *size == n_name => Some((i, p.name.clone())),
            _ => None,
        })?;
    Some(WhereInfo { idx_param, arr_param, idx_pos, arr_pos })
}

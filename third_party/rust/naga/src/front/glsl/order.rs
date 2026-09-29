use crate::{Arena, Block, Expression, Function, Handle, Module, Statement};
use alloc::{collections::BTreeMap, string::String, vec::Vec};

fn walk(block: &mut Block, callback: &mut impl FnMut(&mut Handle<Function>)) {
    for statement in block.iter_mut() {
        match statement {
            Statement::Call { function, .. } => callback(function),
            Statement::Block(inner) => walk(inner, callback),
            Statement::If { accept, reject, .. } => {
                walk(accept, callback);
                walk(reject, callback);
            }
            Statement::Switch { cases, .. } => {
                for case in cases {
                    walk(&mut case.body, callback);
                }
            }
            Statement::Loop {
                body, continuing, ..
            } => {
                walk(body, callback);
                walk(continuing, callback);
            }
            _ => {}
        }
    }
}

pub(super) fn order_functions(module: &mut Module) -> Result<(), String> {
    let mut edges = BTreeMap::new();
    for (handle, function) in module.functions.iter() {
        let mut dependencies = Vec::new();
        walk(&mut function.body.clone(), &mut |h| dependencies.push(*h));
        edges.insert(handle, dependencies);
    }
    fn visit(
        h: Handle<Function>,
        edges: &BTreeMap<Handle<Function>, Vec<Handle<Function>>>,
        states: &mut BTreeMap<Handle<Function>, u8>,
        order: &mut Vec<Handle<Function>>,
    ) -> Result<(), String> {
        match states.get(&h) {
            Some(2) => return Ok(()),
            Some(1) => return Err("recursive function graph".into()),
            _ => {}
        }
        states.insert(h, 1);
        for &callee in &edges[&h] {
            visit(callee, edges, states, order)?;
        }
        states.insert(h, 2);
        order.push(h);
        Ok(())
    }
    let mut order = Vec::new();
    let mut states = BTreeMap::new();
    for &handle in edges.keys() {
        visit(handle, &edges, &mut states, &mut order)?;
    }
    let old = core::mem::take(&mut module.functions);
    let mut functions = Arena::new();
    let mut remap = BTreeMap::new();
    for handle in order {
        remap.insert(
            handle,
            functions.append(old[handle].clone(), old.get_span(handle)),
        );
    }
    let fix = |function: &mut Function| {
        for (_, expression) in function.expressions.iter_mut() {
            if let Expression::CallResult(handle) = expression {
                *handle = remap[handle];
            }
        }
        walk(&mut function.body, &mut |h| *h = remap[h]);
    };
    for (_, function) in functions.iter_mut() {
        fix(function);
    }
    for entry in &mut module.entry_points {
        fix(&mut entry.function);
    }
    module.functions = functions;
    Ok(())
}

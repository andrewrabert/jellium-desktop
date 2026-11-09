use cef::{ImplListValue, ListValue, sys};

pub(crate) struct BrowserMessage {
    name: String,
    args: Option<ListValue>,
}

impl BrowserMessage {
    pub(crate) fn new(name: String, args: Option<ListValue>) -> Self {
        Self { name, args }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn args(&self) -> Option<&ListValue> {
        self.args.as_ref()
    }
}

pub(crate) fn list_string(args: &ListValue, idx: usize) -> String {
    crate::cef_string::userfree_to_string(&args.string(idx))
}

pub(crate) fn list_opt_string(args: &ListValue, idx: usize) -> Option<String> {
    if args.get_type(idx).as_ref() == &sys::cef_value_type_t::VTYPE_STRING {
        Some(list_string(args, idx))
    } else {
        None
    }
}

/// JS can send integers as `VTYPE_DOUBLE` (e.g. via `parseFloat`); round to i32 in that case.
pub(crate) fn list_int(args: &ListValue, idx: usize) -> i32 {
    let t = args.get_type(idx);
    if t.as_ref() == &sys::cef_value_type_t::VTYPE_DOUBLE {
        args.double(idx).round() as i32
    } else {
        args.int(idx)
    }
}

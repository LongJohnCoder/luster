use std::fmt::{self, Debug};
use std::hash::{Hash, Hasher};

use gc_arena::{Collect, Gc, MutationContext};

use crate::{ContinuationResult, Error, Value};

pub type CallbackResult<'gc> = Result<ContinuationResult<'gc, Vec<Value<'gc>>, Error>, Error>;

#[derive(Collect)]
#[collect(require_static)]
pub struct CallbackFn(pub Box<for<'gc> Fn(&[Value<'gc>]) -> CallbackResult<'gc> + 'static>);

impl Debug for CallbackFn {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_tuple("CallbackFn")
            .field(&(&self.0 as *const _))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Collect)]
#[collect(require_copy)]
pub struct Callback<'gc>(pub Gc<'gc, CallbackFn>);

impl<'gc> Callback<'gc> {
    pub fn new<F>(mc: MutationContext<'gc, '_>, f: F) -> Callback<'gc>
    where
        F: 'static + for<'fgc> Fn(&[Value<'fgc>]) -> CallbackResult<'fgc>,
    {
        Callback(Gc::allocate(mc, CallbackFn(Box::new(f))))
    }

    pub fn call(
        &self,
        args: &[Value<'gc>],
    ) -> Result<ContinuationResult<'gc, Vec<Value<'gc>>, Error>, Error> {
        (*(self.0).0)(args)
    }
}

impl<'gc> PartialEq for Callback<'gc> {
    fn eq(&self, other: &Callback<'gc>) -> bool {
        Gc::ptr_eq(&self.0, &other.0)
    }
}

impl<'gc> Eq for Callback<'gc> {}

impl<'gc> Hash for Callback<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (&*self.0 as *const CallbackFn).hash(state)
    }
}

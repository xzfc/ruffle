//! Activation frames

use crate::avm2::class::Class;
use crate::avm2::function::FunctionObject;
use crate::avm2::method::BytecodeMethod;
use crate::avm2::names::{Multiname, Namespace, QName};
use crate::avm2::object::{Object, TObject};
use crate::avm2::scope::Scope;
use crate::avm2::script::Script;
use crate::avm2::script_object::ScriptObject;
use crate::avm2::string::AvmString;
use crate::avm2::value::Value;
use crate::avm2::{value, Avm2, Error};
use crate::context::UpdateContext;
use gc_arena::{Collect, Gc, GcCell, MutationContext};
use smallvec::SmallVec;
use std::io::Cursor;
use swf::avm2::read::Reader;
use swf::avm2::types::{
    Class as AbcClass, Index, Method as AbcMethod, Multiname as AbcMultiname,
    Namespace as AbcNamespace, Op,
};

/// Represents a particular register set.
///
/// This type exists primarily because SmallVec isn't garbage-collectable.
#[derive(Clone)]
pub struct RegisterSet<'gc>(SmallVec<[Value<'gc>; 8]>);

unsafe impl<'gc> gc_arena::Collect for RegisterSet<'gc> {
    #[inline]
    fn trace(&self, cc: gc_arena::CollectionContext) {
        for register in &self.0 {
            register.trace(cc);
        }
    }
}

impl<'gc> RegisterSet<'gc> {
    /// Create a new register set with a given number of specified registers.
    ///
    /// The given registers will be set to `undefined`.
    pub fn new(num: u32) -> Self {
        Self(smallvec![Value::Undefined; num as usize])
    }

    /// Return a reference to a given register, if it exists.
    pub fn get(&self, num: u32) -> Option<&Value<'gc>> {
        self.0.get(num as usize)
    }

    /// Return a mutable reference to a given register, if it exists.
    pub fn get_mut(&mut self, num: u32) -> Option<&mut Value<'gc>> {
        self.0.get_mut(num as usize)
    }
}

#[derive(Debug, Clone)]
enum FrameControl<'gc> {
    Continue,
    Return(Value<'gc>),
}

/// Represents a single activation of a given AVM2 function or keyframe.
#[derive(Collect)]
#[collect(no_drop)]
pub struct Activation<'a, 'gc: 'a, 'gc_context: 'a> {
    /// The immutable value of `this`.
    this: Option<Object<'gc>>,

    /// The arguments this function was called by.
    arguments: Option<Object<'gc>>,

    /// Flags that the current activation frame is being executed and has a
    /// reader object copied from it. Taking out two readers on the same
    /// activation frame is a programming error.
    is_executing: bool,

    /// Local registers.
    ///
    /// All activations have local registers, but it is possible for multiple
    /// activations (such as a rescope) to execute from the same register set.
    local_registers: GcCell<'gc, RegisterSet<'gc>>,

    /// What was returned from the function.
    ///
    /// A return value of `None` indicates that the called function is still
    /// executing. Functions that do not return instead return `Undefined`.
    return_value: Option<Value<'gc>>,

    /// The current local scope, implemented as a bare object.
    local_scope: Object<'gc>,

    /// The current scope stack.
    ///
    /// A `scope` of `None` indicates that the scope stack is empty.
    scope: Option<GcCell<'gc, Scope<'gc>>>,

    /// The base prototype of `this`.
    ///
    /// This will not be available if this is not a method call.
    base_proto: Option<Object<'gc>>,

    pub context: UpdateContext<'a, 'gc, 'gc_context>,
}

impl<'a, 'gc, 'gc_context> Activation<'a, 'gc, 'gc_context> {
    /// Construct an activation that does not represent any particular scope.
    ///
    /// This exists primarily for non-AVM2 related manipulations of the
    /// interpreter environment that require an activation. For example,
    /// loading traits into an object, or running tests.
    ///
    /// It is a logic error to attempt to run AVM2 code in a nothing
    /// `Activation`.
    pub fn from_nothing(context: UpdateContext<'a, 'gc, 'gc_context>) -> Self {
        let local_registers = GcCell::allocate(context.gc_context, RegisterSet::new(0));

        Self {
            this: None,
            arguments: None,
            is_executing: false,
            local_registers,
            return_value: None,
            local_scope: ScriptObject::bare_object(context.gc_context),
            scope: None,
            base_proto: None,
            context,
        }
    }

    /// Construct an activation for the execution of a particular script's
    /// initializer method.
    pub fn from_script(
        context: UpdateContext<'a, 'gc, 'gc_context>,
        script: GcCell<'gc, Script<'gc>>,
        global: Object<'gc>,
    ) -> Result<Self, Error> {
        let method = script.read().init().into_bytecode()?;
        let scope = Some(Scope::push_scope(None, global, context.gc_context));
        let body: Result<_, Error> = method
            .body()
            .ok_or_else(|| "Cannot execute non-native method (for script) without body".into());
        let num_locals = body?.num_locals;
        let local_registers =
            GcCell::allocate(context.gc_context, RegisterSet::new(num_locals + 1));

        *local_registers
            .write(context.gc_context)
            .get_mut(0)
            .unwrap() = global.into();

        Ok(Self {
            this: Some(global),
            arguments: None,
            is_executing: false,
            local_registers,
            return_value: None,
            local_scope: ScriptObject::bare_object(context.gc_context),
            scope,
            base_proto: None,
            context,
        })
    }

    /// Construct an activation for the execution of a particular bytecode
    /// method.
    pub fn from_method(
        context: UpdateContext<'a, 'gc, 'gc_context>,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        scope: Option<GcCell<'gc, Scope<'gc>>>,
        this: Option<Object<'gc>>,
        arguments: &[Value<'gc>],
        base_proto: Option<Object<'gc>>,
    ) -> Result<Self, Error> {
        let body: Result<_, Error> = method
            .body()
            .ok_or_else(|| "Cannot execute non-native method without body".into());
        let num_locals = body?.num_locals;
        let num_declared_arguments = method.method().params.len() as u32;
        let local_registers = GcCell::allocate(
            context.gc_context,
            RegisterSet::new(num_locals + num_declared_arguments + 1),
        );

        {
            let mut write = local_registers.write(context.gc_context);
            *write.get_mut(0).unwrap() = this.map(|t| t.into()).unwrap_or(Value::Null);

            for i in 0..num_declared_arguments {
                *write.get_mut(1 + i).unwrap() = arguments
                    .get(i as usize)
                    .cloned()
                    .unwrap_or(Value::Undefined);
            }
        }

        Ok(Self {
            this,
            arguments: None,
            is_executing: false,
            local_registers,
            return_value: None,
            local_scope: ScriptObject::bare_object(context.gc_context),
            scope,
            base_proto,
            context,
        })
    }

    /// Execute a script initializer.
    pub fn run_stack_frame_for_script(
        &mut self,
        script: GcCell<'gc, Script<'gc>>,
    ) -> Result<(), Error> {
        let init = script.read().init().into_bytecode()?;

        self.run_actions(init)?;

        Ok(())
    }

    /// Attempts to lock the activation frame for execution.
    ///
    /// If this frame is already executing, that is an error condition.
    pub fn lock(&mut self) -> Result<(), Error> {
        if self.is_executing {
            return Err("Attempted to execute the same frame twice".into());
        }

        self.is_executing = true;

        Ok(())
    }

    /// Unlock the activation object. This allows future execution to run on it
    /// again.
    pub fn unlock_execution(&mut self) {
        self.is_executing = false;
    }

    /// Retrieve a local register.
    pub fn local_register(&self, id: u32) -> Result<Value<'gc>, Error> {
        self.local_registers
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("Out of bounds register read: {}", id).into())
    }

    /// Get the current scope stack.
    pub fn scope(&self) -> Option<GcCell<'gc, Scope<'gc>>> {
        self.scope
    }

    /// Set a new scope stack.
    pub fn set_scope(&mut self, new_scope: Option<GcCell<'gc, Scope<'gc>>>) {
        self.scope = new_scope;
    }

    /// Set a local register.
    ///
    /// Returns `true` if the set was successful; `false` otherwise
    pub fn set_local_register(
        &mut self,
        id: u32,
        value: impl Into<Value<'gc>>,
        mc: MutationContext<'gc, '_>,
    ) -> Result<(), Error> {
        if let Some(r) = self.local_registers.write(mc).get_mut(id) {
            *r = value.into();

            Ok(())
        } else {
            Err(format!("Out of bounds register write: {}", id).into())
        }
    }

    pub fn avm2(&mut self) -> &mut Avm2<'gc> {
        self.context.avm2
    }

    /// Set the return value.
    pub fn set_return_value(&mut self, value: Value<'gc>) {
        self.return_value = Some(value);
    }

    /// Get the base prototype of the object that the currently executing
    /// method was retrieved from, if one exists.
    pub fn base_proto(&self) -> Option<Object<'gc>> {
        self.base_proto
    }

    /// Retrieve a int from the current constant pool.
    fn pool_int(
        &self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<i32>,
    ) -> Result<i32, Error> {
        value::abc_int(method.translation_unit(), index)
    }

    /// Retrieve a int from the current constant pool.
    fn pool_uint(
        &self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<u32>,
    ) -> Result<u32, Error> {
        value::abc_uint(method.translation_unit(), index)
    }

    /// Retrieve a double from the current constant pool.
    fn pool_double(
        &self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<f64>,
    ) -> Result<f64, Error> {
        value::abc_double(method.translation_unit(), index)
    }

    /// Retrieve a string from the current constant pool.
    fn pool_string<'b>(
        &self,
        method: &'b BytecodeMethod<'gc>,
        index: Index<String>,
        mc: MutationContext<'gc, '_>,
    ) -> Result<AvmString<'gc>, Error> {
        method.translation_unit().pool_string(index.0, mc)
    }

    /// Retrieve a namespace from the current constant pool.
    fn pool_namespace(
        &self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcNamespace>,
        mc: MutationContext<'gc, '_>,
    ) -> Result<Namespace<'gc>, Error> {
        Namespace::from_abc_namespace(method.translation_unit(), index, mc)
    }

    /// Retrieve a multiname from the current constant pool.
    fn pool_multiname(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        mc: MutationContext<'gc, '_>,
    ) -> Result<Multiname<'gc>, Error> {
        Multiname::from_abc_multiname(method.translation_unit(), index, self.context.avm2, mc)
    }

    /// Retrieve a static, or non-runtime, multiname from the current constant
    /// pool.
    fn pool_multiname_static(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        mc: MutationContext<'gc, '_>,
    ) -> Result<Multiname<'gc>, Error> {
        Multiname::from_abc_multiname_static(method.translation_unit(), index, mc)
    }

    /// Retrieve a method entry from the current ABC file's method table.
    fn table_method(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMethod>,
        mc: MutationContext<'gc, '_>,
    ) -> Result<Gc<'gc, BytecodeMethod<'gc>>, Error> {
        BytecodeMethod::from_method_index(method.translation_unit(), index.clone(), mc)
            .ok_or_else(|| format!("Method index {} does not exist", index.0).into())
    }

    /// Retrieve a class entry from the current ABC file's method table.
    fn table_class(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcClass>,
    ) -> Result<GcCell<'gc, Class<'gc>>, Error> {
        method
            .translation_unit()
            .load_class(index.0, self.context.gc_context)
    }

    pub fn run_actions(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
    ) -> Result<Value<'gc>, Error> {
        let body: Result<_, Error> = method
            .body()
            .ok_or_else(|| "Cannot execute non-native method without body".into());
        let mut read = Reader::new(Cursor::new(body?.code.as_ref()));

        loop {
            let result = self.do_next_opcode(method, &mut read);
            match result {
                Ok(FrameControl::Return(value)) => break Ok(value),
                Ok(FrameControl::Continue) => {}
                Err(e) => break Err(e),
            }
        }
    }

    /// Run a single action from a given action reader.
    fn do_next_opcode(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        reader: &mut Reader<Cursor<&[u8]>>,
    ) -> Result<FrameControl<'gc>, Error> {
        let op = reader.read_op();
        if let Ok(Some(op)) = op {
            avm_debug!(self.avm2(), "Opcode: {:?}", op);

            let result = match op {
                Op::PushByte { value } => self.op_push_byte(value),
                Op::PushDouble { value } => self.op_push_double(method, value),
                Op::PushFalse => self.op_push_false(),
                Op::PushInt { value } => self.op_push_int(method, value),
                Op::PushNamespace { value } => self.op_push_namespace(method, value),
                Op::PushNaN => self.op_push_nan(),
                Op::PushNull => self.op_push_null(),
                Op::PushShort { value } => self.op_push_short(value),
                Op::PushString { value } => self.op_push_string(method, value),
                Op::PushTrue => self.op_push_true(),
                Op::PushUint { value } => self.op_push_uint(method, value),
                Op::PushUndefined => self.op_push_undefined(),
                Op::Pop => self.op_pop(),
                Op::Dup => self.op_dup(),
                Op::GetLocal { index } => self.op_get_local(index),
                Op::SetLocal { index } => self.op_set_local(index),
                Op::Kill { index } => self.op_kill(index),
                Op::Call { num_args } => self.op_call(num_args),
                Op::CallMethod { index, num_args } => self.op_call_method(index, num_args),
                Op::CallProperty { index, num_args } => {
                    self.op_call_property(method, index, num_args)
                }
                Op::CallPropLex { index, num_args } => {
                    self.op_call_prop_lex(method, index, num_args)
                }
                Op::CallPropVoid { index, num_args } => {
                    self.op_call_prop_void(method, index, num_args)
                }
                Op::CallStatic { index, num_args } => self.op_call_static(method, index, num_args),
                Op::CallSuper { index, num_args } => self.op_call_super(method, index, num_args),
                Op::CallSuperVoid { index, num_args } => {
                    self.op_call_super_void(method, index, num_args)
                }
                Op::ReturnValue => self.op_return_value(),
                Op::ReturnVoid => self.op_return_void(),
                Op::GetProperty { index } => self.op_get_property(method, index),
                Op::SetProperty { index } => self.op_set_property(method, index),
                Op::InitProperty { index } => self.op_init_property(method, index),
                Op::DeleteProperty { index } => self.op_delete_property(method, index),
                Op::GetSuper { index } => self.op_get_super(method, index),
                Op::SetSuper { index } => self.op_set_super(method, index),
                Op::PushScope => self.op_push_scope(),
                Op::PushWith => self.op_push_with(),
                Op::PopScope => self.op_pop_scope(),
                Op::GetScopeObject { index } => self.op_get_scope_object(index),
                Op::GetGlobalScope => self.op_get_global_scope(),
                Op::FindProperty { index } => self.op_find_property(method, index),
                Op::FindPropStrict { index } => self.op_find_prop_strict(method, index),
                Op::GetLex { index } => self.op_get_lex(method, index),
                Op::GetSlot { index } => self.op_get_slot(index),
                Op::SetSlot { index } => self.op_set_slot(index),
                Op::GetGlobalSlot { index } => self.op_get_global_slot(index),
                Op::SetGlobalSlot { index } => self.op_set_global_slot(index),
                Op::Construct { num_args } => self.op_construct(num_args),
                Op::ConstructProp { index, num_args } => {
                    self.op_construct_prop(method, index, num_args)
                }
                Op::ConstructSuper { num_args } => self.op_construct_super(num_args),
                Op::NewActivation => self.op_new_activation(),
                Op::NewObject { num_args } => self.op_new_object(num_args),
                Op::NewFunction { index } => self.op_new_function(method, index),
                Op::NewClass { index } => self.op_new_class(method, index),
                Op::CoerceA => self.op_coerce_a(),
                Op::Jump { offset } => self.op_jump(offset, reader),
                Op::IfTrue { offset } => self.op_if_true(offset, reader),
                Op::IfFalse { offset } => self.op_if_false(offset, reader),
                Op::IfStrictEq { offset } => self.op_if_strict_eq(offset, reader),
                Op::IfStrictNe { offset } => self.op_if_strict_ne(offset, reader),
                Op::StrictEquals => self.op_strict_equals(),
                Op::HasNext => self.op_has_next(),
                Op::HasNext2 {
                    object_register,
                    index_register,
                } => self.op_has_next_2(object_register, index_register),
                Op::NextName => self.op_next_name(),
                Op::NextValue => self.op_next_value(),
                Op::IsType { index } => self.op_is_type(method, index),
                Op::IsTypeLate => self.op_is_type_late(),
                Op::InstanceOf => self.op_instance_of(),
                Op::Label => Ok(FrameControl::Continue),
                Op::Debug {
                    is_local_register,
                    register_name,
                    register,
                } => self.op_debug(method, is_local_register, register_name, register),
                Op::DebugFile { file_name } => self.op_debug_file(method, file_name),
                Op::DebugLine { line_num } => self.op_debug_line(line_num),
                _ => self.unknown_op(op),
            };

            if let Err(e) = result {
                log::error!("AVM2 error: {}", e);
                return Err(e);
            }
            result
        } else if let Ok(None) = op {
            log::error!("Unknown opcode!");
            Err("Unknown opcode!".into())
        } else if let Err(e) = op {
            log::error!("Parse error: {:?}", e);
            Err(e.into())
        } else {
            unreachable!();
        }
    }

    fn unknown_op(&mut self, op: swf::avm2::types::Op) -> Result<FrameControl<'gc>, Error> {
        log::error!("Unknown AVM2 opcode: {:?}", op);
        Err("Unknown op".into())
    }

    fn op_push_byte(&mut self, value: u8) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(value);
        Ok(FrameControl::Continue)
    }

    fn op_push_double(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        value: Index<f64>,
    ) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(self.pool_double(method, value)?);
        Ok(FrameControl::Continue)
    }

    fn op_push_false(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(false);
        Ok(FrameControl::Continue)
    }

    fn op_push_int(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        value: Index<i32>,
    ) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(self.pool_int(method, value)?);
        Ok(FrameControl::Continue)
    }

    fn op_push_namespace(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        value: Index<AbcNamespace>,
    ) -> Result<FrameControl<'gc>, Error> {
        self.context
            .avm2
            .push(self.pool_namespace(method, value, self.context.gc_context)?);
        Ok(FrameControl::Continue)
    }

    fn op_push_nan(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(std::f64::NAN);
        Ok(FrameControl::Continue)
    }

    fn op_push_null(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(Value::Null);
        Ok(FrameControl::Continue)
    }

    fn op_push_short(&mut self, value: u32) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(value);
        Ok(FrameControl::Continue)
    }

    fn op_push_string(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        value: Index<String>,
    ) -> Result<FrameControl<'gc>, Error> {
        self.context
            .avm2
            .push(self.pool_string(&method, value, self.context.gc_context)?);
        Ok(FrameControl::Continue)
    }

    fn op_push_true(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(true);
        Ok(FrameControl::Continue)
    }

    fn op_push_uint(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        value: Index<u32>,
    ) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(self.pool_uint(method, value)?);
        Ok(FrameControl::Continue)
    }

    fn op_push_undefined(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(Value::Undefined);
        Ok(FrameControl::Continue)
    }

    fn op_pop(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.pop();

        Ok(FrameControl::Continue)
    }

    fn op_dup(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(
            self.context
                .avm2
                .stack
                .last()
                .cloned()
                .unwrap_or(Value::Undefined),
        );

        Ok(FrameControl::Continue)
    }

    fn op_get_local(&mut self, register_index: u32) -> Result<FrameControl<'gc>, Error> {
        self.context.avm2.push(self.local_register(register_index)?);
        Ok(FrameControl::Continue)
    }

    fn op_set_local(&mut self, register_index: u32) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop();

        self.set_local_register(register_index, value, self.context.gc_context)?;

        Ok(FrameControl::Continue)
    }

    fn op_kill(&mut self, register_index: u32) -> Result<FrameControl<'gc>, Error> {
        self.set_local_register(register_index, Value::Undefined, self.context.gc_context)?;

        Ok(FrameControl::Continue)
    }

    fn op_call(&mut self, arg_count: u32) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let receiver = self.context.avm2.pop().as_object().ok();
        let function = self.context.avm2.pop().as_object()?;
        let base_proto = receiver.and_then(|r| r.proto());
        let value = function.call(receiver, &args, self, base_proto)?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_call_method(
        &mut self,
        index: Index<AbcMethod>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let receiver = self.context.avm2.pop().as_object()?;
        let function: Result<Object<'gc>, Error> = receiver
            .get_method(index.0)
            .ok_or_else(|| format!("Object method {} does not exist", index.0).into());
        let base_proto = receiver.proto();
        let value = function?.call(Some(receiver), &args, self, base_proto)?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_call_property(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let mut receiver = self.context.avm2.pop().as_object()?;
        let name: Result<QName, Error> = receiver
            .resolve_multiname(&multiname)?
            .ok_or_else(|| format!("Could not find method {:?}", multiname.local_name()).into());
        let name = name?;
        let base_proto = receiver.get_base_proto(&name)?;
        let function = receiver.get_property(receiver, &name, self)?.as_object()?;
        let value = function.call(Some(receiver), &args, self, base_proto)?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_call_prop_lex(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let mut receiver = self.context.avm2.pop().as_object()?;
        let name: Result<QName, Error> = receiver
            .resolve_multiname(&multiname)?
            .ok_or_else(|| format!("Could not find method {:?}", multiname.local_name()).into());
        let function = receiver.get_property(receiver, &name?, self)?.as_object()?;
        let value = function.call(None, &args, self, None)?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_call_prop_void(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let mut receiver = self.context.avm2.pop().as_object()?;
        let name: Result<QName, Error> = receiver
            .resolve_multiname(&multiname)?
            .ok_or_else(|| format!("Could not find method {:?}", multiname.local_name()).into());
        let name = name?;
        let base_proto = receiver.get_base_proto(&name)?;
        let function = receiver.get_property(receiver, &name, self)?.as_object()?;

        function.call(Some(receiver), &args, self, base_proto)?;

        Ok(FrameControl::Continue)
    }

    fn op_call_static(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMethod>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let receiver = self.context.avm2.pop().as_object()?;
        let method = self.table_method(method, index, self.context.gc_context)?;
        let scope = self.scope(); //TODO: Is this correct?
        let function = FunctionObject::from_method(
            self.context.gc_context,
            method.into(),
            scope,
            self.context.avm2.prototypes().function,
            None,
        );
        let value = function.call(Some(receiver), &args, self, receiver.proto())?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_call_super(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let receiver = self.context.avm2.pop().as_object()?;
        let name: Result<QName, Error> = receiver
            .resolve_multiname(&multiname)?
            .ok_or_else(|| format!("Could not find method {:?}", multiname.local_name()).into());
        let base_proto: Result<Object<'gc>, Error> =
            self.base_proto().and_then(|bp| bp.proto()).ok_or_else(|| {
                "Attempted to call super method without a superclass."
                    .to_string()
                    .into()
            });
        let base_proto = base_proto?;
        let mut base = base_proto.construct(self, &[])?; //TODO: very hacky workaround

        let function = base.get_property(receiver, &name?, self)?.as_object()?;

        let value = function.call(Some(receiver), &args, self, Some(base_proto))?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_call_super_void(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let receiver = self.context.avm2.pop().as_object()?;
        let name: Result<QName, Error> = receiver
            .resolve_multiname(&multiname)?
            .ok_or_else(|| format!("Could not find method {:?}", multiname.local_name()).into());
        let base_proto: Result<Object<'gc>, Error> =
            self.base_proto().and_then(|bp| bp.proto()).ok_or_else(|| {
                "Attempted to call super method without a superclass."
                    .to_string()
                    .into()
            });
        let base_proto = base_proto?;
        let mut base = base_proto.construct(self, &[])?; //TODO: very hacky workaround

        let function = base.get_property(receiver, &name?, self)?.as_object()?;

        function.call(Some(receiver), &args, self, Some(base_proto))?;

        Ok(FrameControl::Continue)
    }

    fn op_return_value(&mut self) -> Result<FrameControl<'gc>, Error> {
        let return_value = self.context.avm2.pop();

        Ok(FrameControl::Return(return_value))
    }

    fn op_return_void(&mut self) -> Result<FrameControl<'gc>, Error> {
        Ok(FrameControl::Return(Value::Undefined))
    }

    fn op_get_property(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let mut object = self.context.avm2.pop().as_object()?;

        let name: Result<QName, Error> = object.resolve_multiname(&multiname)?.ok_or_else(|| {
            format!("Could not resolve property {:?}", multiname.local_name()).into()
        });

        let value = object.get_property(object, &name?, self)?;
        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_set_property(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop();
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let mut object = self.context.avm2.pop().as_object()?;

        if let Some(name) = object.resolve_multiname(&multiname)? {
            object.set_property(object, &name, value, self)?;
        } else {
            //TODO: Non-dynamic objects should fail
            //TODO: This should only work if the public namespace is present
            let local_name: Result<AvmString<'gc>, Error> = multiname
                .local_name()
                .ok_or_else(|| "Cannot set property using any name".into());
            let name = QName::dynamic_name(local_name?);
            object.set_property(object, &name, value, self)?;
        }

        Ok(FrameControl::Continue)
    }

    fn op_init_property(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop();
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let mut object = self.context.avm2.pop().as_object()?;

        if let Some(name) = object.resolve_multiname(&multiname)? {
            object.init_property(object, &name, value, self)?;
        } else {
            //TODO: Non-dynamic objects should fail
            //TODO: This should only work if the public namespace is present
            let local_name: Result<AvmString<'gc>, Error> = multiname
                .local_name()
                .ok_or_else(|| "Cannot set property using any name".into());
            let name = QName::dynamic_name(local_name?);
            object.init_property(object, &name, value, self)?;
        }

        Ok(FrameControl::Continue)
    }

    fn op_delete_property(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let object = self.context.avm2.pop().as_object()?;

        if let Some(name) = object.resolve_multiname(&multiname)? {
            self.context
                .avm2
                .push(object.delete_property(self.context.gc_context, &name))
        } else {
            self.context.avm2.push(false)
        }

        Ok(FrameControl::Continue)
    }

    fn op_get_super(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let object = self.context.avm2.pop().as_object()?;
        let base_proto: Result<Object<'gc>, Error> = self
            .base_proto()
            .and_then(|p| p.proto())
            .ok_or_else(|| "Attempted to get property on non-existent super object".into());
        let base_proto = base_proto?;
        let mut base = base_proto.construct(self, &[])?; //TODO: very hacky workaround

        let name: Result<QName, Error> = base.resolve_multiname(&multiname)?.ok_or_else(|| {
            format!(
                "Could not resolve {:?} as super property",
                multiname.local_name()
            )
            .into()
        });

        let value = base.get_property(object, &name?, self)?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_set_super(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop();
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let object = self.context.avm2.pop().as_object()?;
        let base_proto: Result<Object<'gc>, Error> = self
            .base_proto()
            .and_then(|p| p.proto())
            .ok_or_else(|| "Attempted to get property on non-existent super object".into());
        let base_proto = base_proto?;
        let mut base = base_proto.construct(self, &[])?; //TODO: very hacky workaround

        let name: Result<QName, Error> = base.resolve_multiname(&multiname)?.ok_or_else(|| {
            format!(
                "Could not resolve {:?} as super property",
                multiname.local_name()
            )
            .into()
        });

        base.set_property(object, &name?, value, self)?;

        Ok(FrameControl::Continue)
    }

    fn op_push_scope(&mut self) -> Result<FrameControl<'gc>, Error> {
        let object = self.context.avm2.pop().as_object()?;
        let scope_stack = self.scope();
        let new_scope = Scope::push_scope(scope_stack, object, self.context.gc_context);

        self.set_scope(Some(new_scope));

        Ok(FrameControl::Continue)
    }

    fn op_push_with(&mut self) -> Result<FrameControl<'gc>, Error> {
        let object = self.context.avm2.pop().as_object()?;
        let scope_stack = self.scope();
        let new_scope = Scope::push_with(scope_stack, object, self.context.gc_context);

        self.set_scope(Some(new_scope));

        Ok(FrameControl::Continue)
    }

    fn op_pop_scope(&mut self) -> Result<FrameControl<'gc>, Error> {
        let scope_stack = self.scope();
        let new_scope = scope_stack.and_then(|s| s.read().pop_scope());

        self.set_scope(new_scope);

        Ok(FrameControl::Continue)
    }

    fn op_get_scope_object(&mut self, mut index: u8) -> Result<FrameControl<'gc>, Error> {
        let mut scope = self.scope();

        while index > 0 {
            if let Some(child_scope) = scope {
                scope = child_scope.read().parent_cell();
            }

            index -= 1;
        }

        self.context.avm2.push(
            scope
                .map(|s| s.read().locals().clone().into())
                .unwrap_or(Value::Undefined),
        );

        Ok(FrameControl::Continue)
    }

    fn op_get_global_scope(&mut self) -> Result<FrameControl<'gc>, Error> {
        let mut scope = self.scope();

        while let Some(this_scope) = scope {
            let parent = this_scope.read().parent_cell();
            if parent.is_none() {
                break;
            }

            scope = parent;
        }

        self.context.avm2.push(
            scope
                .map(|s| s.read().locals().clone().into())
                .unwrap_or(Value::Undefined),
        );

        Ok(FrameControl::Continue)
    }

    fn op_find_property(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        avm_debug!(self.avm2(), "Resolving {:?}", multiname);
        let result = if let Some(scope) = self.scope() {
            scope.read().find(&multiname, self)?
        } else {
            None
        };

        self.context
            .avm2
            .push(result.map(|o| o.into()).unwrap_or(Value::Undefined));

        Ok(FrameControl::Continue)
    }

    fn op_find_prop_strict(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        avm_debug!(self.avm2(), "Resolving {:?}", multiname);
        let found: Result<Object<'gc>, Error> = if let Some(scope) = self.scope() {
            scope.read().find(&multiname, self)?
        } else {
            None
        }
        .ok_or_else(|| format!("Property does not exist: {:?}", multiname.local_name()).into());
        let result: Value<'gc> = found?.into();

        self.context.avm2.push(result);

        Ok(FrameControl::Continue)
    }

    fn op_get_lex(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let multiname = self.pool_multiname_static(method, index, self.context.gc_context)?;
        avm_debug!(self.avm2(), "Resolving {:?}", multiname);
        let found: Result<Value<'gc>, Error> = if let Some(scope) = self.scope() {
            scope
                .write(self.context.gc_context)
                .resolve(&multiname, self)?
        } else {
            None
        }
        .ok_or_else(|| format!("Property does not exist: {:?}", multiname.local_name()).into());
        let result: Value<'gc> = found?;

        self.context.avm2.push(result);

        Ok(FrameControl::Continue)
    }

    fn op_get_slot(&mut self, index: u32) -> Result<FrameControl<'gc>, Error> {
        let object = self.context.avm2.pop().as_object()?;
        let value = object.get_slot(index)?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_set_slot(&mut self, index: u32) -> Result<FrameControl<'gc>, Error> {
        let object = self.context.avm2.pop().as_object()?;
        let value = self.context.avm2.pop();

        object.set_slot(index, value, self.context.gc_context)?;

        Ok(FrameControl::Continue)
    }

    fn op_get_global_slot(&mut self, index: u32) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.globals().get_slot(index)?;

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_set_global_slot(&mut self, index: u32) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop();

        self.context
            .avm2
            .globals()
            .set_slot(index, value, self.context.gc_context)?;

        Ok(FrameControl::Continue)
    }

    fn op_construct(&mut self, arg_count: u32) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let mut ctor = self.context.avm2.pop().as_object()?;

        let proto = ctor
            .get_property(
                ctor,
                &QName::new(Namespace::public_namespace(), "prototype"),
                self,
            )?
            .as_object()?;

        let object = proto.construct(self, &args)?;
        ctor.call(Some(object), &args, self, object.proto())?;

        self.context.avm2.push(object);

        Ok(FrameControl::Continue)
    }

    fn op_construct_prop(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMultiname>,
        arg_count: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let multiname = self.pool_multiname(method, index, self.context.gc_context)?;
        let mut source = self.context.avm2.pop().as_object()?;

        let ctor_name: Result<QName, Error> =
            source.resolve_multiname(&multiname)?.ok_or_else(|| {
                format!("Could not resolve property {:?}", multiname.local_name()).into()
            });
        let mut ctor = source
            .get_property(source, &ctor_name?, self)?
            .as_object()?;
        let proto = ctor
            .get_property(
                ctor,
                &QName::new(Namespace::public_namespace(), "prototype"),
                self,
            )?
            .as_object()?;

        let object = proto.construct(self, &args)?;
        ctor.call(Some(object), &args, self, Some(proto))?;

        self.context.avm2.push(object);

        Ok(FrameControl::Continue)
    }

    fn op_construct_super(&mut self, arg_count: u32) -> Result<FrameControl<'gc>, Error> {
        let args = self.context.avm2.pop_args(arg_count);
        let receiver = self.context.avm2.pop().as_object()?;
        let name = QName::new(Namespace::public_namespace(), "constructor");
        let base_proto: Result<Object<'gc>, Error> =
            self.base_proto().and_then(|p| p.proto()).ok_or_else(|| {
                "Attempted to call super constructor without a superclass."
                    .to_string()
                    .into()
            });
        let mut base_proto = base_proto?;

        let function = base_proto
            .get_property(receiver, &name, self)?
            .as_object()?;

        function.call(Some(receiver), &args, self, Some(base_proto))?;

        Ok(FrameControl::Continue)
    }

    fn op_new_activation(&mut self) -> Result<FrameControl<'gc>, Error> {
        self.context
            .avm2
            .push(ScriptObject::bare_object(self.context.gc_context));

        Ok(FrameControl::Continue)
    }

    fn op_new_object(&mut self, num_args: u32) -> Result<FrameControl<'gc>, Error> {
        let mut object = ScriptObject::object(
            self.context.gc_context,
            self.context.avm2.prototypes().object,
        );

        for _ in 0..num_args {
            let value = self.context.avm2.pop();
            let name = self.context.avm2.pop();

            object.set_property(
                object,
                &QName::new(Namespace::public_namespace(), name.as_string()?),
                value,
                self,
            )?;
        }

        self.context.avm2.push(object);

        Ok(FrameControl::Continue)
    }

    fn op_new_function(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcMethod>,
    ) -> Result<FrameControl<'gc>, Error> {
        let method_entry = self.table_method(method, index, self.context.gc_context)?;
        let scope = self.scope();

        let mut new_fn = FunctionObject::from_method(
            self.context.gc_context,
            method_entry.into(),
            scope,
            self.context.avm2.prototypes().function,
            None,
        );
        let es3_proto = ScriptObject::object(
            self.context.gc_context,
            self.context.avm2.prototypes().object,
        );

        new_fn.install_slot(
            self.context.gc_context,
            QName::new(Namespace::public_namespace(), "prototype"),
            0,
            es3_proto.into(),
        );

        self.context.avm2.push(new_fn);

        Ok(FrameControl::Continue)
    }

    fn op_new_class(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        index: Index<AbcClass>,
    ) -> Result<FrameControl<'gc>, Error> {
        let base_value = self.context.avm2.pop();
        let base_class = match base_value {
            Value::Object(o) => Some(o),
            Value::Null => None,
            _ => return Err("Base class for new class is not Object or null.".into()),
        };

        let class_entry = self.table_class(method, index)?;
        let scope = self.scope();

        let (new_class, class_init) =
            FunctionObject::from_class(self, class_entry, base_class, scope)?;

        class_init.call(Some(new_class), &[], self, None)?;

        self.context.avm2.push(new_class);

        Ok(FrameControl::Continue)
    }

    fn op_coerce_a(&mut self) -> Result<FrameControl<'gc>, Error> {
        Ok(FrameControl::Continue)
    }

    fn op_jump(
        &mut self,
        offset: i32,
        reader: &mut Reader<Cursor<&[u8]>>,
    ) -> Result<FrameControl<'gc>, Error> {
        reader.seek(offset as i64)?;

        Ok(FrameControl::Continue)
    }

    fn op_if_true(
        &mut self,
        offset: i32,
        reader: &mut Reader<Cursor<&[u8]>>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop().as_bool()?;

        if value {
            reader.seek(offset as i64)?;
        }

        Ok(FrameControl::Continue)
    }

    fn op_if_false(
        &mut self,
        offset: i32,
        reader: &mut Reader<Cursor<&[u8]>>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop().as_bool()?;

        if !value {
            reader.seek(offset as i64)?;
        }

        Ok(FrameControl::Continue)
    }

    fn op_if_strict_eq(
        &mut self,
        offset: i32,
        reader: &mut Reader<Cursor<&[u8]>>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value2 = self.context.avm2.pop();
        let value1 = self.context.avm2.pop();

        if value1 == value2 {
            reader.seek(offset as i64)?;
        }

        Ok(FrameControl::Continue)
    }

    fn op_if_strict_ne(
        &mut self,
        offset: i32,
        reader: &mut Reader<Cursor<&[u8]>>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value2 = self.context.avm2.pop();
        let value1 = self.context.avm2.pop();

        if value1 != value2 {
            reader.seek(offset as i64)?;
        }

        Ok(FrameControl::Continue)
    }

    fn op_strict_equals(&mut self) -> Result<FrameControl<'gc>, Error> {
        let value2 = self.context.avm2.pop();
        let value1 = self.context.avm2.pop();

        self.context.avm2.push(value1 == value2);

        Ok(FrameControl::Continue)
    }

    fn op_has_next(&mut self) -> Result<FrameControl<'gc>, Error> {
        //TODO: After adding ints, change this to ints.
        let cur_index = self.context.avm2.pop().as_number()?;
        let object = self.context.avm2.pop().as_object()?;

        let next_index = cur_index as u32 + 1;

        if object.get_enumerant_name(next_index).is_some() {
            self.context.avm2.push(next_index as f32);
        } else {
            self.context.avm2.push(0.0);
        }

        Ok(FrameControl::Continue)
    }

    fn op_has_next_2(
        &mut self,
        object_register: u32,
        index_register: u32,
    ) -> Result<FrameControl<'gc>, Error> {
        //TODO: After adding ints, change this to ints.
        let cur_index = self.local_register(index_register)?.as_number()?;
        let mut object = Some(self.local_register(object_register)?.as_object()?);

        let mut next_index = cur_index as u32 + 1;

        while let Some(cur_object) = object {
            if cur_object.get_enumerant_name(next_index).is_none() {
                next_index = 1;
                object = cur_object.proto();
            } else {
                break;
            }
        }

        if object.is_none() {
            next_index = 0;
        }

        self.context.avm2.push(next_index != 0);
        self.set_local_register(index_register, next_index, self.context.gc_context)?;
        self.set_local_register(
            object_register,
            object.map(|v| v.into()).unwrap_or(Value::Null),
            self.context.gc_context,
        )?;

        Ok(FrameControl::Continue)
    }

    fn op_next_name(&mut self) -> Result<FrameControl<'gc>, Error> {
        //TODO: After adding ints, change this to ints.
        let cur_index = self.context.avm2.pop().as_number()?;
        let object = self.context.avm2.pop().as_object()?;

        let name = object
            .get_enumerant_name(cur_index as u32)
            .map(|n| n.local_name().into());

        self.context.avm2.push(name.unwrap_or(Value::Undefined));

        Ok(FrameControl::Continue)
    }

    fn op_next_value(&mut self) -> Result<FrameControl<'gc>, Error> {
        //TODO: After adding ints, change this to ints.
        let cur_index = self.context.avm2.pop().as_number()?;
        let mut object = self.context.avm2.pop().as_object()?;

        let name = object.get_enumerant_name(cur_index as u32);
        let value = if let Some(name) = name {
            object.get_property(object, &name, self)?
        } else {
            Value::Undefined
        };

        self.context.avm2.push(value);

        Ok(FrameControl::Continue)
    }

    fn op_is_type(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        type_name_index: Index<AbcMultiname>,
    ) -> Result<FrameControl<'gc>, Error> {
        let value = self.context.avm2.pop().as_object()?;

        let type_name =
            self.pool_multiname_static(method, type_name_index, self.context.gc_context)?;
        let type_object = if let Some(scope) = self.scope() {
            scope.read().find(&type_name, self)?
        } else {
            None
        };

        if let Some(type_object) = type_object {
            let is_instance_of = value.is_instance_of(self, type_object, true)?;
            self.context.avm2.push(is_instance_of);
        } else {
            return Err(format!(
                "Attempted to check against nonexistent type {:?}",
                type_name
            )
            .into());
        }

        Ok(FrameControl::Continue)
    }

    fn op_is_type_late(&mut self) -> Result<FrameControl<'gc>, Error> {
        let type_object = self.context.avm2.pop().as_object()?;
        let value = self.context.avm2.pop().as_object()?;

        let is_instance_of = value.is_instance_of(self, type_object, true)?;

        self.context.avm2.push(is_instance_of);

        Ok(FrameControl::Continue)
    }

    fn op_instance_of(&mut self) -> Result<FrameControl<'gc>, Error> {
        let type_object = self.context.avm2.pop().as_object()?;
        let value = self.context.avm2.pop().as_object()?;

        let is_instance_of = value.is_instance_of(self, type_object, false)?;

        self.context.avm2.push(is_instance_of);

        Ok(FrameControl::Continue)
    }

    #[allow(unused_variables)]
    #[cfg(avm_debug)]
    fn op_debug(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        is_local_register: bool,
        register_name: Index<String>,
        register: u8,
    ) -> Result<FrameControl<'gc>, Error> {
        if is_local_register {
            let register_name = self.pool_string(method, register_name)?;
            let value = self.local_register(register as u32)?;

            avm_debug!(self.avm2(), "Debug: {} = {:?}", register_name, value);
        } else {
            avm_debug!(self.avm2(), "Unknown debugging mode!");
        }

        Ok(FrameControl::Continue)
    }

    #[allow(unused_variables)]
    #[cfg(not(avm_debug))]
    fn op_debug(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        is_local_register: bool,
        register_name: Index<String>,
        register: u8,
    ) -> Result<FrameControl<'gc>, Error> {
        Ok(FrameControl::Continue)
    }

    #[allow(unused_variables)]
    #[cfg(avm_debug)]
    fn op_debug_file(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        file_name: Index<String>,
    ) -> Result<FrameControl<'gc>, Error> {
        let file_name = self.pool_string(method, file_name)?;

        avm_debug!(self.avm2(), "File: {}", file_name);

        Ok(FrameControl::Continue)
    }

    #[allow(unused_variables)]
    #[cfg(not(avm_debug))]
    fn op_debug_file(
        &mut self,
        method: Gc<'gc, BytecodeMethod<'gc>>,
        file_name: Index<String>,
    ) -> Result<FrameControl<'gc>, Error> {
        Ok(FrameControl::Continue)
    }

    #[allow(unused_variables)]
    fn op_debug_line(&mut self, line_num: u32) -> Result<FrameControl<'gc>, Error> {
        avm_debug!(self.avm2(), "Line: {}", line_num);

        Ok(FrameControl::Continue)
    }
}

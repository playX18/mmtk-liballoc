use std::marker::PhantomData;

use super::slot::*;
use crate::{
    objectmodel::{header::HeapObjectHeader, reference::*, vtable::*},
    runtime::threads::Thread,
    MMTKVMKit, Runtime, SlotOf, ThreadOf, VTableOf,
};
use flume::{Receiver, Sender};
use mmtk::{
    util::{Address, ObjectReference},
    vm::{slot::Slot, ObjectTracer, Scanning},
    MutatorContext,
};

pub struct VMScanning<R: Runtime> {
    pub(crate) weak_callbacks_tx: Sender<(
        ObjectReference,
        Box<dyn FnOnce(ObjectReference, &mut Tracer<R>)>,
    )>,
    pub(crate) weak_callbacks_rx: Receiver<(
        ObjectReference,
        Box<dyn FnOnce(ObjectReference, &mut Tracer<R>)>,
    )>,
}

impl<R: Runtime> Default for VMScanning<R> {
    fn default() -> Self {
        let (tx, rx) = flume::unbounded();

        Self {
            weak_callbacks_rx: rx,
            weak_callbacks_tx: tx,
        }
    }
}

impl<R: Runtime> Scanning<MMTKVMKit<R>> for VMScanning<R> {
    fn support_slot_enqueuing(
        _tls: mmtk::util::VMWorkerThread,
        object: mmtk::util::ObjectReference,
    ) -> bool {
        let object = <&HeapObjectHeader<R>>::from(object);
        let vt = VTableOf::<R>::from_pointer(object.vtable()).gc();
        matches!(vt.trace, TraceCallback::ScanSlots(_)) && VTableOf::<R>::ENQUEUE_VTABLE
    }

    fn scan_object<SV: mmtk::vm::SlotVisitor<<MMTKVMKit<R> as mmtk::vm::VMBinding>::VMSlot>>(
        _tls: mmtk::util::VMWorkerThread,
        object: mmtk::util::ObjectReference,
        slot_visitor: &mut SV,
    ) {
        println!("scanning object {} slots", object.to_raw_address());
        let header = <&HeapObjectHeader<R>>::from(object);

        let vt = VTableOf::<R>::from_pointer(header.vtable()).gc();

        if VTableOf::<R>::VTALBE_IS_OBJECT {
            slot_visitor.visit_slot(SlotOf::<R>::from_vtable_slot(VTableSlot::<R>::new(object)));
        }

        let TraceCallback::ScanSlots(scan) = vt.trace else {
            unreachable!()
        };
        let mut sv = |slot| slot_visitor.visit_slot(slot);
        let mut vis = Visitor {
            sv: &mut sv as &mut dyn FnMut(R::Slot),
            source: object,
        };

        scan(object, &mut vis);
    }

    fn scan_object_and_trace_edges<OT: mmtk::vm::ObjectTracer>(
        _tls: mmtk::util::VMWorkerThread,
        object: mmtk::util::ObjectReference,
        object_tracer: &mut OT,
    ) {
        let header = <&HeapObjectHeader<R>>::from(object);

        if VTableOf::<R>::VTALBE_IS_OBJECT {
            if let Some(vtable_object) = VTableOf::<R>::to_object_reference(header.vtable()) {
                let new_vtable = object_tracer.trace_object(vtable_object);
                header.set_vtable(VTableOf::<R>::from_object_reference(new_vtable));
            }
        }

        let vt = VTableOf::<R>::from_pointer(header.vtable()).gc();

        match vt.trace {
            TraceCallback::ScanObjects(scan) => {
                let mut sv = |objref| {
                    let new = object_tracer.trace_object(objref);

                    new
                };

                let mut vis = Tracer {
                    marker: PhantomData,
                    sv: &mut sv,
                    source: object,
                };

                scan(object, &mut vis);
            }

            /*
               "simulate" slot enqueing by updating slots in-place.

               This is required when `VTABLE_IS_OBJECT` is set to true, main reason for this
               is that we can't create object tracer inside `scan_slots` method, once it's possible in
               MMTK this piece of code should be removed

            */
            TraceCallback::ScanSlots(scan) => {
                let mut sv = |slot: SlotOf<R>| {
                    if let Some(object) = slot.load() {
                        slot.store(object_tracer.trace_object(object))
                    }
                };

                let mut vis = Visitor {
                    source: object,
                    sv: &mut sv,
                };
                scan(object, &mut vis);

                return;
            }

            TraceCallback::NoTrace => return,
        }
    }

    fn notify_initial_thread_scan_complete(_partial_scan: bool, _tls: mmtk::util::VMWorkerThread) {}

    fn forward_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<MMTKVMKit<R>>,
        _tracer_context: impl mmtk::vm::ObjectTracerContext<MMTKVMKit<R>>,
    ) {
    }

    fn prepare_for_roots_re_scanning() {}

    fn process_weak_refs(
        worker: &mut mmtk::scheduler::GCWorker<MMTKVMKit<R>>,
        tracer_context: impl mmtk::vm::ObjectTracerContext<MMTKVMKit<R>>,
    ) -> bool {
        let mut rescan = false;

        tracer_context.with_tracer(worker, |tracer| {
            let mut v = |objref| {
                rescan = true;
                tracer.trace_object(objref)
            };
            for (obj, weak_callback) in R::vmkit().scanning.weak_callbacks_rx.drain() {
                weak_callback(
                    obj,
                    &mut Tracer {
                        marker: PhantomData,
                        sv: &mut v,
                        source: obj,
                    },
                );
            }
        });

        R::process_weak_refs(worker, tracer_context) || rescan
    }

    fn scan_roots_in_mutator_thread(
        _tls: mmtk::util::VMWorkerThread,
        mutator: &'static mut mmtk::Mutator<MMTKVMKit<R>>,
        factory: impl mmtk::vm::RootsWorkFactory<<MMTKVMKit<R> as mmtk::vm::VMBinding>::VMSlot>,
    ) {
        let tls = mutator.get_tls();
        mutator.flush();

        ThreadOf::<R>::scan_roots(tls, factory);
    }

    fn scan_vm_specific_roots(
        _tls: mmtk::util::VMWorkerThread,
        factory: impl mmtk::vm::RootsWorkFactory<<MMTKVMKit<R> as mmtk::vm::VMBinding>::VMSlot>,
    ) {
        R::scan_roots(factory);
    }

    fn supports_return_barrier() -> bool {
        false
    }
}

pub struct Visitor<'a, R: Runtime> {
    sv: &'a mut dyn FnMut(R::Slot),
    source: ObjectReference,
}

impl<'a, R: Runtime> Visitor<'a, R> {
    pub fn visit_member<T, Tag: 'static>(&mut self, member: &BasicMember<T, Tag>) {
        if std::any::TypeId::of::<Tag>() == std::any::TypeId::of::<StrongMemberTag>() {
            let slot = member.slot::<R>();
            (self.sv)(slot);
        } else if std::any::TypeId::of::<Tag>() == std::any::TypeId::of::<WeakMemberTag>() {
            let offset = Address::from_ref(member) - self.source.to_raw_address();

            self.register_weak_callback(Box::new(move |objref, _tracer| unsafe {
                let raw = objref.to_raw_address();
                let field = raw + offset;
                let member = field.as_mut_ref::<BasicMember<T, WeakMemberTag>>();

                if let Some(objref) = member
                    .object_reference::<R>()
                    .filter(|objref| objref.is_reachable())
                {
                    member.write(Some(objref.get_forwarded_object().unwrap_or(objref)));
                } else {
                    member.write(None);
                }
            }));
        }
    }

    pub fn visit_slot(&mut self, slot: R::Slot) {
        (self.sv)(slot);
    }

    pub fn register_weak_callback(
        &mut self,
        callback: Box<dyn FnOnce(ObjectReference, &mut Tracer<R>)>,
    ) {
        R::vmkit()
            .scanning
            .weak_callbacks_tx
            .send((self.source, callback))
            .unwrap();
    }
}

#[allow(dead_code)]
pub struct Tracer<'a, R: Runtime> {
    sv: &'a mut dyn FnMut(ObjectReference) -> ObjectReference,
    source: ObjectReference,
    marker: PhantomData<R>,
}

impl<'a, R: Runtime> Tracer<'a, R> {
    pub fn trace_member<'gc, T, Tag: 'static>(
        &mut self,
        member: BasicMember<'gc, T, Tag>,
    ) -> BasicMember<'gc, T, Tag> {
        if std::any::TypeId::of::<Tag>() == std::any::TypeId::of::<StrongMemberTag>() {
            if let Some(objref) = member.object_reference::<R>() {
                BasicMember::from_object_reference::<R>((self.sv)(objref))
            } else {
                member
            }
        } else if std::any::TypeId::of::<Tag>() == std::any::TypeId::of::<WeakMemberTag>() {
            panic!(
                "Cannot trace weak member, use `Visitor` or `register_weak_callback` on your own"
            );
        } else {
            // untraced object: skip
            member
        }
    }

    pub fn trace_object_reference(&mut self, objref: ObjectReference) -> ObjectReference {
        (self.sv)(objref)
    }

    pub fn register_weak_callback(
        &mut self,
        object: ObjectReference,
        callback: Box<dyn FnOnce(ObjectReference, &mut Tracer<R>)>,
    ) {
        R::vmkit()
            .scanning
            .weak_callbacks_tx
            .send((object, callback))
            .unwrap();
    }
}

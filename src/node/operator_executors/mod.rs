//! Traits and implementations for executors that enable each operator to run.
//!
//! Each type of operator defined in src/dataflow/operator.rs requires a corresponding executor to
//! be implemented in this module. This executor defines how the operator handles notifications
//! from the worker channels, and invokes its corresponding callbacks upon received messages.
//!
//! TODO (Sukrit): Define how to utilize the OperatorExecutorT and OneInMessageProcessorT traits.

// Export the executors outside.
mod source_executor;
mod sink_executor;
mod one_in_one_out_executor;
mod two_in_one_out_executor;
mod one_in_two_out_executor;

pub use source_executor::*;
pub use sink_executor::*;
pub use one_in_one_out_executor::*;
pub use two_in_one_out_executor::*;
pub use one_in_two_out_executor::*;

/* ***********************************************************************************************
 * Imports for the traits.
 * ***********************************************************************************************/
use std::{cmp, collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use futures_delay_queue::{delay_queue, DelayHandle, DelayQueue, Receiver,};
use futures_intrusive::buffer::GrowingHeapBuf;
use serde::Deserialize;
use tokio::{
    self,
    sync::{broadcast, mpsc},
};

use crate::{
    dataflow::{
        deadlines::{Deadline, DeadlineEvent},
        operator::SetupContextT,
        stream::StreamId,
        Data, Message, ReadStream, StreamT, Timestamp,
    },
    node::{
        lattice::ExecutionLattice,
        operator_event::OperatorEvent,
        worker::{EventNotification, OperatorExecutorNotification, WorkerNotification},
    },
    OperatorId,
};

/* ***********************************************************************************************
 * Traits that need to be defined by the executor for each operator type.
 * ***********************************************************************************************/

/// Trait that needs to be defined by the executors for each operator. This trait helps the workers
/// to execute the different types of operators in the system and merge their execution lattices.
pub(crate) trait OperatorExecutorT: Send {
    /// Returns a future for OperatorExecutor::execute().
    fn execute<'a>(
        &'a mut self,
        channel_from_worker: broadcast::Receiver<OperatorExecutorNotification>,
        channel_to_worker: mpsc::UnboundedSender<WorkerNotification>,
        channel_to_event_runners: broadcast::Sender<EventNotification>,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a + Send>>;

    /// Returns the lattice into which the executor inserts events.
    fn lattice(&self) -> Arc<ExecutionLattice>;

    /// Returns the operator ID.
    fn operator_id(&self) -> OperatorId;
}

/// Trait that needs to be defined by the executors for an operator that processes a single message
/// stream. This trait is used by the OperatorExecutorHelper to invoke the executor when a message
/// is received on the channel from the worker.
pub trait OneInMessageProcessorT<T>: Send + Sync
where
    T: Data + for<'a> Deserialize<'a>,
{
    /// Generates an OperatorEvent for a message callback.
    fn message_cb_event(&mut self, msg: Arc<Message<T>>) -> OperatorEvent;

    /// Generates an OperatorEvent for a watermark callback.
    fn watermark_cb_event(&mut self, timestamp: &Timestamp) -> OperatorEvent;

    /// Generates a DeadlineEvent for arming a deadline.
    fn arm_deadlines(
        &self,
        setup_context: &dyn SetupContextT,
        read_stream: &ReadStream<T>,
        timestamp: Timestamp,
    ) -> Vec<DeadlineEvent> {
        let mut deadline_event_vec = Vec::new();
        for deadline in setup_context.get_deadlines() {
            match deadline {
                Deadline::TimestampDeadline(d) => {
                    if d.constrained_on_read_stream(read_stream.id())
                        && d.start_condition(read_stream.get_condition_context(), &timestamp)
                    {
                        // Compute the deadline for the timestamp.
                        let deadline_duration =
                            d.calculate_deadline(read_stream.get_condition_context());
                        deadline_event_vec.push(DeadlineEvent::new(
                            read_stream.id(),
                            timestamp.clone(),
                            deadline_duration,
                            d.get_handler(),
                            d.get_end_condition_fn(),
                        ));
                    }
                }
            }
        }
        deadline_event_vec
    }

    /// Disarms a deadline by returning true if the given deadline should be disarmed, or false
    /// otherwise.
    fn disarm_deadline(&self, _deadline_event: &DeadlineEvent) -> bool {
        true
    }
}

/// Trait that needs to be defined by the executors for an operator that processes two message
/// streams. This trait is used by the OperatorExecutorHelper to invoke the executor when a message
/// is received on the channel from the worker for either of the two streams.
pub trait TwoInMessageProcessorT<T, U>: Send + Sync
where
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
{
    /// Generates an OperatorEvent for a stateless callback on the first stream.
    fn left_stateless_cb_event(&self, msg: Arc<Message<T>>) -> OperatorEvent;

    /// Generates an OperatorEvent for a stateful callback on the first stream.
    fn left_stateful_cb_event(&self, msg: Arc<Message<T>>) -> OperatorEvent;

    /// Generates an OperatorEvent for a stateless callback on the second stream.
    fn right_stateless_cb_event(&self, msg: Arc<Message<U>>) -> OperatorEvent;

    /// Generates an OperatorEvent for a stateful callback on the second stream.
    fn right_stateful_cb_event(&self, msg: Arc<Message<U>>) -> OperatorEvent;

    /// Generates an OperatorEvent for a watermark callback.
    fn watermark_cb_event(&self, timestamp: &Timestamp) -> OperatorEvent;
}

/* ***********************************************************************************************
 * Helper structures.
 * ***********************************************************************************************/

pub struct OperatorExecutorHelper {
    operator_id: OperatorId,
    lattice: Arc<ExecutionLattice>,
    _event_runner_handles: Option<Vec<tokio::task::JoinHandle<()>>>,
    deadline_queue: DelayQueue<DeadlineEvent, GrowingHeapBuf<DeadlineEvent>>,
    deadline_queue_rx: Receiver<DeadlineEvent>,
    // For active deadlines.
    stream_timestamp_to_key_map: HashMap<(StreamId, Timestamp), DelayHandle>,
}

impl OperatorExecutorHelper {
    fn new(operator_id: OperatorId) -> Self {
        let (deadline_queue, deadline_queue_rx) = delay_queue();
        OperatorExecutorHelper {
            operator_id,
            lattice: Arc::new(ExecutionLattice::new()),
            _event_runner_handles: None,
            deadline_queue,
            deadline_queue_rx,
            stream_timestamp_to_key_map: HashMap::new(),
        }
    }

    async fn synchronize(&self) {
        // TODO: replace this with a synchronization step
        // that ensures all operators are ready to run.
        tokio::time::delay_for(Duration::from_secs(1)).await;
    }

    // Arms the given `DeadlineEvents` by installing them into a DeadlineQueue.
    fn manage_deadlines(&mut self, deadlines: Vec<DeadlineEvent>) {
        for event in deadlines {
            if !self
                .stream_timestamp_to_key_map
                .contains_key(&(event.stream_id, event.timestamp.clone()))
            {
                // Install the handler onto the queue with the given duration.
                let event_duration = event.duration;
                let stream_id = event.stream_id;
                let event_timestamp = event.timestamp.clone();
                let queue_key: DelayHandle = self.deadline_queue.insert(event, event_duration);
                slog::debug!(
                    crate::TERMINAL_LOGGER,
                    "Installed a deadline handler with the DelayHandle: {:?} corresponding to \
                            Stream ID: {} and Timestamp: {:?}",
                    queue_key,
                    stream_id,
                    event_timestamp,
                );

                self.stream_timestamp_to_key_map
                    .insert((stream_id, event_timestamp), queue_key);
            }
        }
    }

    async fn process_stream<T>(
        &mut self,
        mut read_stream: ReadStream<T>,
        message_processor: &mut dyn OneInMessageProcessorT<T>,
        notifier_tx: &tokio::sync::broadcast::Sender<EventNotification>,
        setup_context: &dyn SetupContextT,
    ) where
        T: Data + for<'a> Deserialize<'a>,
    {
        loop {
            tokio::select! {
                // DelayQueue returns `None` if the queue is empty. This means that if there are no
                // deadlines installed, the queue will always be ready and return `None` thus
                // wasting resources. We can potentially fix this by inserting a Deadline for the
                // future and maintaining it so that the queue is not empty.
                Some(deadline_event) = self.deadline_queue_rx.receive() => {
                    // Missed a deadline. Check if the end condition is satisfied and invoke the
                    // handler if not so.
                    // TODO (Sukrit): The handler is invoked in the thread of the OperatorExecutor.
                    // This may be an issue for long-running handlers since they block the
                    // processing of future messages. We can spawn these as a separate task.
                    // let deadline_event = event.unwrap().into_inner();
                    if !message_processor.disarm_deadline(&deadline_event) {
                        // Invoke the handler.
                        deadline_event.handler.lock().unwrap().invoke_handler(
                            &read_stream.get_condition_context(),
                            &deadline_event.timestamp.clone()
                        );
                    }

                    // Remove the key from the hashmap and clear the state in the ConditionContext.
                    match self.stream_timestamp_to_key_map.remove(
                        &(deadline_event.stream_id, deadline_event.timestamp.clone())) {
                        None => {
                            slog::warn!(
                                crate::TERMINAL_LOGGER,
                                "Could not find a key corresponding to the Stream ID: {} \
                                and the Timestamp: {:?}",
                                deadline_event.stream_id,
                                deadline_event.timestamp,
                            );
                        }
                        Some(key) => {
                            slog::debug!(
                                crate::TERMINAL_LOGGER,
                                "Finished invoking the deadline handler for the DelayHandle: {:?} \
                                corresponding to the Stream ID: {} and the Timestamp: {:?}",
                                key, deadline_event.stream_id, deadline_event.timestamp);
                        }
                    }
                },
                // If there is a message on the ReadStream, then increment the messgae counts for
                // the given timestamp, evaluate the start and end condition and install / disarm
                // deadlines accordingly.
                // TODO (Sukrit) : The start and end conditions are evaluated in the thread of the
                // OperatorExecutor, and can be moved to a separate task if they become a
                // bottleneck.
                Ok(msg) = read_stream.async_read() => {
                    let events = match msg.data() {
                        // Data message
                        Some(_) => {
                            // TODO : Check if an event for both the stateful and the stateless
                            // callback is needed.

                            // Stateless callback.
                            let msg_ref = Arc::clone(&msg);
                            let stateless_data_event = message_processor.message_cb_event(
                                msg_ref,
                            );

                            vec![stateless_data_event]
                        },

                        // Watermark
                        None => {
                            let watermark_event = message_processor.watermark_cb_event(
                                msg.timestamp());
                            vec![watermark_event]
                        }
                    };

                    // Arm deadlines and install them into the executor.
                    let deadline_events = message_processor.arm_deadlines(
                        setup_context,
                        &read_stream,
                        msg.timestamp().clone()
                    );
                    self.manage_deadlines(deadline_events);

                    self.lattice.add_events(events).await;
                    notifier_tx
                        .send(EventNotification::AddedEvents(self.operator_id))
                        .unwrap();
                },
                else => break,
            }
        }
    }

    async fn process_two_streams<T, U>(
        &self,
        mut left_read_stream: ReadStream<T>,
        mut right_read_stream: ReadStream<U>,
        message_processor: &dyn TwoInMessageProcessorT<T, U>,
        notifier_tx: &tokio::sync::broadcast::Sender<EventNotification>,
    ) where
        T: Data + for<'a> Deserialize<'a>,
        U: Data + for<'a> Deserialize<'a>,
    {
        let mut left_watermark = Timestamp::Bottom;
        let mut right_watermark = Timestamp::Bottom;
        let mut min_watermark = cmp::min(&left_watermark, &right_watermark).clone();
        loop {
            let events = tokio::select! {
                Ok(left_msg) = left_read_stream.async_read() => {
                    match left_msg.data() {
                        // Data message
                        Some(_) => {
                            // Stateless callback.
                            let msg_ref = Arc::clone(&left_msg);
                            let data_event = message_processor.left_stateless_cb_event(msg_ref);

                            // Stateful callback
                            let msg_ref = Arc::clone(&left_msg);
                            let stateful_data_event = message_processor.left_stateful_cb_event(
                                msg_ref,
                            );
                            vec![data_event, stateful_data_event]
                        }
                        // Watermark
                        None => {
                            left_watermark = left_msg.timestamp().clone();
                            let advance_watermark = cmp::min(
                                &left_watermark,
                                &right_watermark,
                            ) > &min_watermark;
                            if advance_watermark {
                                min_watermark = left_watermark.clone();
                                vec![message_processor.watermark_cb_event(
                                    &left_msg.timestamp().clone())]
                            } else {
                                Vec::new()
                            }
                        }
                    }
                }
                Ok(right_msg) = right_read_stream.async_read() => {
                    match right_msg.data() {
                        // Data message
                        Some(_) => {
                            // Stateless callback.
                            let msg_ref = Arc::clone(&right_msg);
                            let data_event = message_processor.right_stateless_cb_event(msg_ref);

                            // Stateful callback
                            let msg_ref = Arc::clone(&right_msg);
                            let stateful_data_event = message_processor.right_stateful_cb_event(
                                msg_ref,
                            );
                            vec![data_event, stateful_data_event]
                        }
                        // Watermark
                        None => {
                            right_watermark = right_msg.timestamp().clone();
                            let advance_watermark = cmp::min(
                                &left_watermark,
                                &right_watermark,
                            ) > &min_watermark;
                            if advance_watermark {
                                min_watermark = right_watermark.clone();
                                vec![message_processor.watermark_cb_event(
                                    &right_msg.timestamp().clone())]
                            } else {
                                Vec::new()
                            }
                        }
                    }
                }
                else => break,
            };
            self.lattice.add_events(events).await;
            notifier_tx
                .send(EventNotification::AddedEvents(self.operator_id))
                .unwrap();
        }
    }
}
use serde::Deserialize;
use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::{
    self,
    sync::{broadcast, mpsc},
};

use crate::{
    dataflow::{
        deadlines::DeadlineEvent,
        operator::{
            OneInOneOut, OneInOneOutContext, OneInOneOutSetupContext, OperatorConfig,
            StatefulOneInOneOutContext,
        },
        stream::WriteStreamT,
        Data, Message, ReadStream, State, StreamT, Timestamp, WriteStream,
    },
    node::{
        lattice::ExecutionLattice,
        operator_event::{OperatorEvent, OperatorType},
        operator_executors::{OneInMessageProcessorT, OperatorExecutorHelper, OperatorExecutorT},
        worker::{EventNotification, OperatorExecutorNotification, WorkerNotification},
    },
    OperatorId, Uuid,
};

pub struct OneInOneOutExecutor<O, S, T, U>
where
    O: OneInOneOut<S, T, U>,
    S: State,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
{
    config: OperatorConfig,
    operator: O,
    state: Arc<Mutex<S>>,
    read_stream: Option<ReadStream<T>>,
    write_stream: WriteStream<U>,
    helper: Option<OperatorExecutorHelper>,
    state_ids: HashSet<Uuid>,
}

impl<O, S, T, U> OneInOneOutExecutor<O, S, T, U>
where
    O: OneInOneOut<S, T, U>,
    S: State,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
{
    pub fn new(
        config: OperatorConfig,
        operator_fn: impl Fn() -> O + Send,
        state_fn: impl Fn() -> S + Send,
        read_stream: ReadStream<T>,
        write_stream: WriteStream<U>,
    ) -> Self {
        let operator_id = config.id;
        Self {
            config,
            operator: operator_fn(),
            state: Arc::new(Mutex::new(state_fn())),
            state_ids: vec![crate::Uuid::new_deterministic()].into_iter().collect(),
            read_stream: Some(read_stream),
            write_stream,
            helper: Some(OperatorExecutorHelper::new(operator_id)),
        }
    }

    pub(crate) async fn execute(
        &mut self,
        mut channel_from_worker: broadcast::Receiver<OperatorExecutorNotification>,
        channel_to_worker: mpsc::UnboundedSender<WorkerNotification>,
        channel_to_event_runners: broadcast::Sender<EventNotification>,
    ) {
        let mut helper = self.helper.take().unwrap();
        helper.synchronize().await;

        slog::debug!(
            crate::TERMINAL_LOGGER,
            "Node {}: running operator {}",
            self.config.node_id,
            self.config.get_name()
        );

        // Run the setup method and save the setup context.
        let mut setup_context =
            OneInOneOutSetupContext::new(self.read_stream.as_ref().unwrap().id());
        self.operator.setup(&mut setup_context);

        tokio::task::block_in_place(|| {
            self.operator
                .run(self.read_stream.as_mut().unwrap(), &mut self.write_stream)
        });

        let read_stream: ReadStream<T> = self.read_stream.take().unwrap();
        let process_stream_fut = helper.process_stream(
            read_stream,
            &mut (*self),
            &channel_to_event_runners,
            &setup_context,
        );

        loop {
            tokio::select! {
                _ = process_stream_fut => break,
                notification_result = channel_from_worker.recv() => {
                    match notification_result {
                        Ok(notification) => {
                            match notification {
                                OperatorExecutorNotification::Shutdown => { break; }
                            }
                        }
                        Err(e) => {
                            slog::error!(crate::get_terminal_logger(),
                            "Operator executor {}: error receiving notifications {:?}", self.operator_id(), e);
                            break;
                        }
                    }
                }
            };
        }

        tokio::task::block_in_place(|| self.operator.destroy());

        // Return the helper.
        self.helper.replace(helper);

        // Close the stream.
        // TODO: check that the top watermark hasn't already been sent.
        if !self.write_stream.is_closed() {
            self.write_stream
                .send(Message::new_watermark(Timestamp::Top))
                .expect(&format!(
                    "[Operator Executor] Error sending top watermark for operator {}",
                    self.config.get_name()
                ));
        }

        channel_to_worker
            .send(WorkerNotification::DestroyedOperator(self.operator_id()))
            .unwrap();
    }
}

impl<O, S, T, U> OperatorExecutorT for OneInOneOutExecutor<O, S, T, U>
where
    O: OneInOneOut<S, T, U>,
    S: State,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
{
    fn execute<'a>(
        &'a mut self,
        channel_from_worker: broadcast::Receiver<OperatorExecutorNotification>,
        channel_to_worker: mpsc::UnboundedSender<WorkerNotification>,
        channel_to_event_runners: broadcast::Sender<EventNotification>,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a + Send>> {
        Box::pin(self.execute(
            channel_from_worker,
            channel_to_worker,
            channel_to_event_runners,
        ))
    }

    fn lattice(&self) -> Arc<ExecutionLattice> {
        Arc::clone(&self.helper.as_ref().unwrap().lattice)
    }

    fn operator_id(&self) -> OperatorId {
        self.config.id
    }
}

impl<O, S, T, U> OneInMessageProcessorT<T> for OneInOneOutExecutor<O, S, T, U>
where
    O: OneInOneOut<S, T, U>,
    S: State,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
{
    fn message_cb_event(&mut self, msg: Arc<Message<T>>) -> OperatorEvent {
        let mut ctx = OneInOneOutContext {
            timestamp: msg.timestamp().clone(),
            config: self.config.clone(),
            write_stream: self.write_stream.clone(),
        };
        OperatorEvent::new(
            msg.timestamp().clone(),
            false,
            0,
            HashSet::new(),
            HashSet::new(),
            move || O::on_data(&mut ctx, msg.data().unwrap()),
            OperatorType::ReadOnly,
        )
    }

    // Generates an OperatorEvent for a watermark callback.
    fn watermark_cb_event(&mut self, timestamp: &Timestamp) -> OperatorEvent {
        let mut ctx = StatefulOneInOneOutContext {
            timestamp: timestamp.clone(),
            config: self.config.clone(),
            write_stream: self.write_stream.clone(),
            state: Arc::clone(&self.state),
        };
        if self.config.flow_watermarks {
            let mut write_stream_copy = self.write_stream.clone();
            let timestamp_copy = timestamp.clone();
            OperatorEvent::new(
                timestamp.clone(),
                true,
                127,
                HashSet::new(),
                self.state_ids.clone(),
                move || {
                    O::on_watermark(&mut ctx);
                    write_stream_copy
                        .send(Message::new_watermark(timestamp_copy))
                        .ok();
                },
                OperatorType::ReadOnly,
            )
        } else {
            OperatorEvent::new(
                timestamp.clone(),
                true,
                0,
                HashSet::new(),
                self.state_ids.clone(),
                move || {
                    O::on_watermark(&mut ctx);
                },
                OperatorType::ReadOnly,
            )
        }
    }

    // Checks if the given deadline should be disarmed.
    fn disarm_deadline(&self, deadline_event: &DeadlineEvent) -> bool {
        (deadline_event.end_condition)(
            &self
                .write_stream
                .get_statistics()
                .lock()
                .unwrap()
                .get_condition_context(),
            &deadline_event.timestamp,
        )
    }
}

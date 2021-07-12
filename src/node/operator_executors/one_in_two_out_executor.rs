use serde::Deserialize;
use std::{
    collections::HashSet,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{
    dataflow::{
        operator::{
            OneInTwoOut, OneInTwoOutContext, OperatorConfig, ParallelOneInTwoOut,
            ParallelOneInTwoOutContext,
        },
        stream::WriteStreamT,
        AppendableStateT, Data, Message, ReadStream, StateT, Timestamp, WriteStream,
    },
    node::{
        operator_event::{OperatorEvent, OperatorType},
        operator_executors::OneInMessageProcessorT,
    },
    Uuid,
};

pub struct ParallelOneInTwoOutMessageProcessor<O, S, T, U, V, W>
where
    O: 'static + ParallelOneInTwoOut<S, T, U, V, W>,
    S: AppendableStateT<W>,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
    V: Data + for<'a> Deserialize<'a>,
{
    config: OperatorConfig,
    operator: Arc<O>,
    state: Arc<S>,
    state_ids: HashSet<Uuid>,
    left_write_stream: WriteStream<U>,
    right_write_stream: WriteStream<V>,
    phantom_t: PhantomData<T>,
    phantom_w: PhantomData<W>,
}

impl<O, S, T, U, V, W> ParallelOneInTwoOutMessageProcessor<O, S, T, U, V, W>
where
    O: 'static + ParallelOneInTwoOut<S, T, U, V, W>,
    S: AppendableStateT<W>,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
    V: Data + for<'a> Deserialize<'a>,
    W: 'static + Send + Sync,
{
    pub fn new(
        config: OperatorConfig,
        operator_fn: impl Fn() -> O + Send,
        state_fn: impl Fn() -> S + Send,
        left_write_stream: WriteStream<U>,
        right_write_stream: WriteStream<V>,
    ) -> Self {
        Self {
            config,
            operator: Arc::new(operator_fn()),
            state: Arc::new(state_fn()),
            state_ids: vec![Uuid::new_deterministic()].into_iter().collect(),
            left_write_stream,
            right_write_stream,
            phantom_t: PhantomData,
            phantom_w: PhantomData,
        }
    }
}

impl<O, S, T, U, V, W> OneInMessageProcessorT<T>
    for ParallelOneInTwoOutMessageProcessor<O, S, T, U, V, W>
where
    O: 'static + ParallelOneInTwoOut<S, T, U, V, W>,
    S: AppendableStateT<W>,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
    V: Data + for<'a> Deserialize<'a>,
    W: 'static + Send + Sync,
{
    fn execute_run(&mut self, read_stream: &mut ReadStream<T>) {
        Arc::get_mut(&mut self.operator).unwrap().run(
            read_stream,
            &mut self.left_write_stream,
            &mut self.right_write_stream,
        );
    }

    fn execute_destroy(&mut self) {
        Arc::get_mut(&mut self.operator).unwrap().destroy();
    }

    fn cleanup(&mut self) {
        if !self.left_write_stream.is_closed() {
            self.left_write_stream
                .send(Message::new_watermark(Timestamp::Top))
                .expect(&format!(
                    "[ParallelOneInTwoOut] Error sending Top watermark on left stream \
                    for operator {}",
                    self.config.get_name()
                ));
        }
        if !self.right_write_stream.is_closed() {
            self.right_write_stream
                .send(Message::new_watermark(Timestamp::Top))
                .expect(&format!(
                    "[ParallelOneInTwoOut] Error sending Top watermark on right stream\
                    for operator {}",
                    self.config.get_name()
                ));
        }
    }

    fn message_cb_event(&mut self, msg: Arc<Message<T>>) -> OperatorEvent {
        // Clone the reference to the operator and the state.
        let operator = Arc::clone(&self.operator);
        let state = Arc::clone(&self.state);
        let time = msg.timestamp().clone();
        let config = self.config.clone();
        let left_write_stream = self.left_write_stream.clone();
        let right_write_stream = self.right_write_stream.clone();

        OperatorEvent::new(
            time.clone(),
            false,
            0,
            HashSet::new(),
            HashSet::new(),
            move || {
                operator.on_data(
                    &ParallelOneInTwoOutContext::new(
                        time,
                        config,
                        &state,
                        left_write_stream,
                        right_write_stream,
                    ),
                    msg.data().unwrap(),
                )
            },
            OperatorType::Parallel,
        )
    }

    fn watermark_cb_event(&mut self, timestamp: &Timestamp) -> OperatorEvent {
        // Clone the reference to the operator and the state.
        let operator = Arc::clone(&self.operator);
        let state = Arc::clone(&self.state);
        let time = timestamp.clone();
        let config = self.config.clone();
        let left_write_stream = self.left_write_stream.clone();
        let right_write_stream = self.right_write_stream.clone();

        if self.config.flow_watermarks {
            let mut left_write_stream_copy = self.left_write_stream.clone();
            let mut right_write_stream_copy = self.right_write_stream.clone();
            let time_copy = time.clone();
            let time_copy_left = time.clone();
            let time_copy_right = time.clone();
            OperatorEvent::new(
                time.clone(),
                true,
                127,
                HashSet::new(),
                self.state_ids.clone(),
                move || {
                    // Invoke the watermark method.
                    operator.on_watermark(&mut ParallelOneInTwoOutContext::new(
                        time,
                        config,
                        &state,
                        left_write_stream,
                        right_write_stream,
                    ));

                    // Send a watermark.
                    left_write_stream_copy
                        .send(Message::new_watermark(time_copy_left))
                        .ok();
                    right_write_stream_copy
                        .send(Message::new_watermark(time_copy_right))
                        .ok();

                    // Commit the state.
                    state.commit(&time_copy);
                },
                OperatorType::Parallel,
            )
        } else {
            OperatorEvent::new(
                time.clone(),
                true,
                0,
                HashSet::new(),
                self.state_ids.clone(),
                move || {
                    // Invoke the watermark method.
                    operator.on_watermark(&mut ParallelOneInTwoOutContext::new(
                        time.clone(),
                        config,
                        &state,
                        left_write_stream,
                        right_write_stream,
                    ));

                    // Commit the state.
                    state.commit(&time);
                },
                OperatorType::Parallel,
            )
        }
    }
}

pub struct OneInTwoOutMessageProcessor<O, S, T, U, V>
where
    O: 'static + OneInTwoOut<S, T, U, V>,
    S: StateT,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
    V: Data + for<'a> Deserialize<'a>,
{
    config: OperatorConfig,
    operator: Arc<Mutex<O>>,
    state: Arc<Mutex<S>>,
    state_ids: HashSet<Uuid>,
    left_write_stream: WriteStream<U>,
    right_write_stream: WriteStream<V>,
    phantom_t: PhantomData<T>,
}

impl<O, S, T, U, V> OneInTwoOutMessageProcessor<O, S, T, U, V>
where
    O: 'static + OneInTwoOut<S, T, U, V>,
    S: StateT,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
    V: Data + for<'a> Deserialize<'a>,
{
    pub fn new(
        config: OperatorConfig,
        operator_fn: impl Fn() -> O + Send,
        state_fn: impl Fn() -> S + Send,
        left_write_stream: WriteStream<U>,
        right_write_stream: WriteStream<V>,
    ) -> Self {
        Self {
            config,
            operator: Arc::new(Mutex::new(operator_fn())),
            state: Arc::new(Mutex::new(state_fn())),
            state_ids: vec![Uuid::new_deterministic()].into_iter().collect(),
            left_write_stream,
            right_write_stream,
            phantom_t: PhantomData,
        }
    }
}

impl<O, S, T, U, V> OneInMessageProcessorT<T> for OneInTwoOutMessageProcessor<O, S, T, U, V>
where
    O: 'static + OneInTwoOut<S, T, U, V>,
    S: StateT,
    T: Data + for<'a> Deserialize<'a>,
    U: Data + for<'a> Deserialize<'a>,
    V: Data + for<'a> Deserialize<'a>,
{
    fn execute_run(&mut self, read_stream: &mut ReadStream<T>) {
        self.operator.lock().unwrap().run(
            read_stream,
            &mut self.left_write_stream,
            &mut self.right_write_stream,
        );
    }

    fn execute_destroy(&mut self) {
        self.operator.lock().unwrap().destroy();
    }

    fn cleanup(&mut self) {
        if !self.left_write_stream.is_closed() {
            self.left_write_stream
                .send(Message::new_watermark(Timestamp::Top))
                .expect(&format!(
                    "[ParallelOneInTwoOut] Error sending Top watermark on left stream \
                    for operator {}",
                    self.config.get_name()
                ));
        }
        if !self.right_write_stream.is_closed() {
            self.right_write_stream
                .send(Message::new_watermark(Timestamp::Top))
                .expect(&format!(
                    "[ParallelOneInTwoOut] Error sending Top watermark on right stream\
                    for operator {}",
                    self.config.get_name()
                ));
        }
    }

    fn message_cb_event(&mut self, msg: Arc<Message<T>>) -> OperatorEvent {
        // Clone the reference to the operator and the state.
        let operator = Arc::clone(&self.operator);
        let state = Arc::clone(&self.state);
        let time = msg.timestamp().clone();
        let config = self.config.clone();
        let left_write_stream = self.left_write_stream.clone();
        let right_write_stream = self.right_write_stream.clone();

        OperatorEvent::new(
            time.clone(),
            false,
            0,
            HashSet::new(),
            HashSet::new(),
            move || {
                operator.lock().unwrap().on_data(
                    &mut OneInTwoOutContext::new(
                        time,
                        config,
                        &mut state.lock().unwrap(),
                        left_write_stream,
                        right_write_stream,
                    ),
                    msg.data().unwrap(),
                )
            },
            OperatorType::Sequential,
        )
    }

    fn watermark_cb_event(&mut self, timestamp: &Timestamp) -> OperatorEvent {
        // Clone the reference to the operator and the state.
        let operator = Arc::clone(&self.operator);
        let state = Arc::clone(&self.state);
        let time = timestamp.clone();
        let config = self.config.clone();
        let left_write_stream = self.left_write_stream.clone();
        let right_write_stream = self.right_write_stream.clone();

        if self.config.flow_watermarks {
            let mut left_write_stream_copy = self.left_write_stream.clone();
            let mut right_write_stream_copy = self.right_write_stream.clone();
            let time_copy = time.clone();
            let time_copy_left = time.clone();
            let time_copy_right = time.clone();
            OperatorEvent::new(
                time.clone(),
                true,
                127,
                HashSet::new(),
                self.state_ids.clone(),
                move || {
                    // Take a lock on the state and the operator and invoke the callback.
                    let mutable_state = &mut state.lock().unwrap();
                    operator
                        .lock()
                        .unwrap()
                        .on_watermark(&mut OneInTwoOutContext::new(
                            time,
                            config,
                            mutable_state,
                            left_write_stream,
                            right_write_stream,
                        ));

                    // Send a watermark.
                    left_write_stream_copy
                        .send(Message::new_watermark(time_copy_left))
                        .ok();
                    right_write_stream_copy
                        .send(Message::new_watermark(time_copy_right))
                        .ok();

                    // Commit the state.
                    mutable_state.commit(&time_copy);
                },
                OperatorType::Sequential,
            )
        } else {
            OperatorEvent::new(
                time.clone(),
                true,
                0,
                HashSet::new(),
                self.state_ids.clone(),
                move || {
                    // Take a lock on the state and the operator and invoke the callback.
                    let mutable_state = &mut state.lock().unwrap();
                    operator
                        .lock()
                        .unwrap()
                        .on_watermark(&mut OneInTwoOutContext::new(
                            time.clone(),
                            config,
                            &mut state.lock().unwrap(),
                            left_write_stream,
                            right_write_stream,
                        ));

                    // Commit the state.
                    mutable_state.commit(&time);
                },
                OperatorType::Sequential,
            )
        }
    }
}

// TODO docs
pub trait SyncBatchReceiver<T> {
    /// Pushes a batch of values to the receiver.
    ///
    /// It first pushes the first slice, then the last slice and finally the `value`.
    ///
    /// It has such an interesting signature because it can be used in ring-based queues.
    fn push_many_and_one(&self, first: &[T], last: &[T], value: T);

    /// Pushes a batch of values to the receiver.
    ///
    /// It first pushes the first slice, then the last slice and finally the `slice`.
    ///
    /// It has such an interesting signature because it can be used in ring-based queues.
    fn push_many_and_slice(&self, first: &[T], last: &[T], slice: &[T]);
}

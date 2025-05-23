use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::errors::JobHandleError;
use super::errors::JobQueueError;
use super::traits::Job;
use super::traits::JobCancelReceiver;
use super::traits::JobCancelSender;
use super::traits::JobCompletion;
use super::traits::JobResult;
use super::traits::JobResultReceiver;
use super::traits::JobResultSender;

/// A job-handle enables cancelling a job and awaiting results
#[derive(Debug)]
pub struct JobHandle {
    result_rx: JobResultReceiver,
    cancel_tx: JobCancelSender,
}
impl JobHandle {
    /// wait for job to complete
    ///
    /// a completed job may either be finished, cancelled, or panicked.
    pub async fn complete(self) -> Result<JobCompletion, JobHandleError> {
        Ok(self.result_rx.await?)
    }

    /// wait for job result, or err if cancelled or a panic occurred within job.
    pub async fn result(self) -> Result<Box<dyn JobResult>, JobHandleError> {
        match self.complete().await? {
            JobCompletion::Finished(r) => Ok(r),
            JobCompletion::Cancelled => Err(JobHandleError::JobCancelled),
            JobCompletion::Panicked(e) => Err(JobHandleError::JobPanicked(e)),
        }
    }

    /// cancel job and return immediately.
    pub fn cancel(&self) -> Result<(), JobHandleError> {
        Ok(self.cancel_tx.send(())?)
    }

    /// cancel job and wait for it to complete.
    pub async fn cancel_and_await(self) -> Result<JobCompletion, JobHandleError> {
        self.cancel_tx.send(())?;
        self.complete().await
    }

    /// channel receiver for job results
    pub fn result_rx(self) -> JobResultReceiver {
        self.result_rx
    }

    /// channel sender for cancelling job.
    pub fn cancel_tx(&self) -> &JobCancelSender {
        &self.cancel_tx
    }
}

/// messages that can be sent to job-queue inner task.
enum JobQueueMsg<P> {
    AddJob(AddJobMsg<P>),
    Stop,
}

/// represents a msg to add a job to the queue.
struct AddJobMsg<P> {
    job: Box<dyn Job>,
    result_tx: JobResultSender,
    cancel_tx: JobCancelSender,
    cancel_rx: JobCancelReceiver,
    priority: P,
}

/// implements a job queue that sends result of each job to a listener.
pub struct JobQueue<P> {
    tx: mpsc::UnboundedSender<JobQueueMsg<P>>,

    process_jobs_task_handle: JoinHandle<()>, // Store the job processing task handle
    add_job_task_handle: JoinHandle<()>,      // store the job addition task handle.
}

// useful for detecting when a receiver gets dropped.
struct LoggedReceiver<T>(UnboundedReceiver<T>);

impl<T> Drop for LoggedReceiver<T> {
    fn drop(&mut self) {
        tracing::debug!("LoggedReceiver dropped!");
    }
}

impl<P> std::fmt::Debug for JobQueue<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobQueue")
            .field("tx", &"mpsc::Sender")
            .finish()
    }
}

impl<P> Drop for JobQueue<P> {
    fn drop(&mut self) {
        tracing::debug!("in JobQueue::drop()");

        let _ = self.tx.send(JobQueueMsg::Stop);

        // not really necessary, but it avoids a dead-code warning.
        self.process_jobs_task_handle.abort();
        self.add_job_task_handle.abort();
    }
}

impl<P: Ord + Send + Sync + 'static> JobQueue<P> {
    /// creates job queue and starts it processing.  returns immediately.
    pub fn start() -> Self {
        struct CurrentJob {
            job_num: usize,
            cancel_tx: JobCancelSender,
        }
        struct Shared<P: Ord> {
            jobs: VecDeque<AddJobMsg<P>>,
            current_job: Option<CurrentJob>,
        }

        let (tx, rx) = mpsc::unbounded_channel::<JobQueueMsg<P>>();
        let mut rx = LoggedReceiver(rx);

        let shared = Shared {
            jobs: VecDeque::new(),
            current_job: None,
        };
        let jobs: Arc<Mutex<Shared<P>>> = Arc::new(Mutex::new(shared));

        let (tx_deque, mut rx_deque) = tokio::sync::mpsc::unbounded_channel();

        // spawns background task that adds incoming jobs to job-queue
        let jobs_rc1 = jobs.clone();
        let add_job_task_handle = tokio::spawn(async move {
            while let Some(msg) = rx.0.recv().await {
                match msg {
                    JobQueueMsg::AddJob(m) => {
                        let (num_jobs, job_running) = {
                            let mut guard = jobs_rc1.lock().unwrap();
                            guard.jobs.push_back(m);
                            let job_running = match &guard.current_job {
                                Some(j) => format!("#{}", j.job_num),
                                None => "none".to_string(),
                            };
                            (guard.jobs.len(), job_running)
                        };
                        tracing::info!(
                            "JobQueue: job added.  {} queued job(s).  job running: {}",
                            num_jobs,
                            job_running
                        );
                        let _ = tx_deque.send(());
                    }
                    JobQueueMsg::Stop => {
                        tracing::info!("JobQueue: received stop message.  stopping.");
                        drop(tx_deque); // close queue

                        // if there is a presently executing job we need to cancel it.
                        let guard = jobs_rc1.lock().unwrap();
                        if let Some(current_job) = &guard.current_job {
                            if !current_job.cancel_tx.is_closed() {
                                current_job.cancel_tx.send(()).unwrap();
                                tracing::info!("JobQueue: notified current job to stop.");
                            }
                        }
                        break;
                    }
                }
            }
            tracing::debug!("task add/stop exiting");
        });

        // spawns background task that processes job queue and runs jobs.
        let jobs_rc2 = jobs.clone();
        let process_jobs_task_handle = tokio::spawn(async move {
            let mut job_num: usize = 1;

            while rx_deque.recv().await.is_some() {
                let (msg, pending) = {
                    let mut guard = jobs_rc2.lock().unwrap();
                    guard
                        .jobs
                        .make_contiguous()
                        .sort_by(|a, b| b.priority.cmp(&a.priority));
                    let job = guard.jobs.pop_front().unwrap();
                    guard.current_job = Some(CurrentJob {
                        job_num,
                        cancel_tx: job.cancel_tx.clone(),
                    });
                    (job, guard.jobs.len())
                };

                tracing::info!(
                    "  *** JobQueue: begin job #{} - {} queued job(s) ***",
                    job_num,
                    pending
                );
                let timer = tokio::time::Instant::now();
                let task_handle = if msg.job.is_async() {
                    tokio::spawn(async move { msg.job.run_async_cancellable(msg.cancel_rx).await })
                } else {
                    tokio::task::spawn_blocking(move || msg.job.run(msg.cancel_rx))
                };

                let job_completion = match task_handle.await {
                    Ok(jc) => jc,
                    Err(e) => {
                        if e.is_panic() {
                            JobCompletion::Panicked(e.into_panic())
                        } else if e.is_cancelled() {
                            JobCompletion::Cancelled
                        } else {
                            unreachable!()
                        }
                    }
                };

                tracing::info!(
                    "  *** JobQueue: ended job #{} - Completion: {} - {} secs ***",
                    job_num,
                    job_completion,
                    timer.elapsed().as_secs_f32()
                );
                job_num += 1;

                jobs_rc2.lock().unwrap().current_job = None;

                let _ = msg.result_tx.send(job_completion);
            }
            tracing::debug!("task process_jobs exiting");
        });

        tracing::info!("JobQueue: started new queue.");

        Self {
            tx,
            process_jobs_task_handle,
            add_job_task_handle,
        }
    }

    /// adds job to job-queue and returns immediately.
    ///
    /// job-results can be obtained by via JobHandle::results().await
    /// The job can be cancelled by JobHandle::cancel()
    pub fn add_job(&self, job: Box<dyn Job>, priority: P) -> Result<JobHandle, JobQueueError> {
        let (result_tx, result_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = watch::channel::<()>(());

        let msg = JobQueueMsg::AddJob(AddJobMsg {
            job,
            result_tx,
            cancel_tx: cancel_tx.clone(),
            cancel_rx: cancel_rx.clone(),
            priority,
        });
        if self.tx.is_closed() {
            tracing::error!("JobQueue::add_job() -- add-job channel is unexpectedly closed!!!");
        }
        self.tx
            .send(msg)
            .map_err(|e| JobQueueError::AddJobError(e.to_string()))?;

        Ok(JobHandle {
            result_rx,
            cancel_tx,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tracing_test::traced_test;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn run_sync_jobs_by_priority() -> anyhow::Result<()> {
        workers::run_jobs_by_priority(false).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn run_async_jobs_by_priority() -> anyhow::Result<()> {
        workers::run_jobs_by_priority(true).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn get_sync_job_result() -> anyhow::Result<()> {
        workers::get_job_result(false).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn get_async_job_result() -> anyhow::Result<()> {
        workers::get_job_result(true).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn cancel_sync_job() -> anyhow::Result<()> {
        workers::cancel_job(false).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn cancel_async_job() -> anyhow::Result<()> {
        workers::cancel_job(true).await
    }

    #[test]
    #[traced_test]
    fn runtime_shutdown_timeout_force_cancels_sync_job() -> anyhow::Result<()> {
        workers::runtime_shutdown_timeout_force_cancels_job(false)
    }

    #[test]
    #[traced_test]
    fn runtime_shutdown_timeout_force_cancels_async_job() -> anyhow::Result<()> {
        workers::runtime_shutdown_timeout_force_cancels_job(true)
    }

    #[test]
    #[traced_test]
    fn runtime_shutdown_cancels_sync_job() {
        let _ = workers::runtime_shutdown_cancels_job(false);
    }

    #[test]
    #[traced_test]
    fn runtime_shutdown_cancels_async_job() -> anyhow::Result<()> {
        workers::runtime_shutdown_cancels_job(true)
    }

    #[test]
    #[traced_test]
    fn spawned_tasks_live_as_long_as_jobqueue() -> anyhow::Result<()> {
        workers::spawned_tasks_live_as_long_as_jobqueue(true)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn panic_in_async_job_ends_job_cleanly() -> anyhow::Result<()> {
        workers::panics::panic_in_job_ends_job_cleanly(true).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn panic_in_blocking_job_ends_job_cleanly() -> anyhow::Result<()> {
        workers::panics::panic_in_job_ends_job_cleanly(false).await
    }

    mod workers {
        use std::any::Any;

        use super::*;
        use crate::job_queue::errors::JobHandleErrorSync;

        #[derive(PartialEq, Eq, PartialOrd, Ord)]
        pub enum DoubleJobPriority {
            Low = 1,
            Medium = 2,
            High = 3,
        }

        #[derive(PartialEq, Debug, Clone)]
        struct DoubleJobResult(u64, u64, Instant);
        impl JobResult for DoubleJobResult {
            fn as_any(&self) -> &dyn Any {
                self
            }
            fn into_any(self: Box<Self>) -> Box<dyn Any> {
                self
            }
        }

        // represents a prover job.  implements Job.
        struct DoubleJob {
            data: u64,
            duration: std::time::Duration,
            is_async: bool,
        }

        #[async_trait::async_trait]
        impl Job for DoubleJob {
            fn is_async(&self) -> bool {
                self.is_async
            }

            fn run(&self, cancel_rx: JobCancelReceiver) -> JobCompletion {
                let start = Instant::now();
                let sleep_time =
                    std::cmp::min(std::time::Duration::from_micros(100), self.duration);

                let r = loop {
                    if start.elapsed() < self.duration {
                        match cancel_rx.has_changed() {
                            Ok(changed) if changed => break JobCompletion::Cancelled,
                            Err(_) => break JobCompletion::Cancelled,
                            _ => {}
                        }

                        std::thread::sleep(sleep_time);
                    } else {
                        break JobCompletion::Finished(Box::new(DoubleJobResult(
                            self.data,
                            self.data * 2,
                            Instant::now(),
                        )));
                    }
                };

                tracing::info!("results: {:?}", r);
                r
            }

            async fn run_async(&self) -> Box<dyn JobResult> {
                tokio::time::sleep(self.duration).await;
                let r = DoubleJobResult(self.data, self.data * 2, Instant::now());

                tracing::info!("results: {:?}", r);
                Box::new(r)
            }
        }

        // this test demonstrates/verifies that:
        //  1. jobs are run in priority order, highest priority first.
        //  2. when multiple jobs have the same priority, they run in FIFO order.
        pub(super) async fn run_jobs_by_priority(is_async: bool) -> anyhow::Result<()> {
            let start_of_test = Instant::now();

            // create a job queue
            let job_queue = JobQueue::start();

            let mut handles = vec![];
            let duration = std::time::Duration::from_millis(20);

            // create 30 jobs, 10 at each priority level.
            for i in (1..10).rev() {
                let job1 = Box::new(DoubleJob {
                    data: i,
                    duration,
                    is_async,
                });
                let job2 = Box::new(DoubleJob {
                    data: i * 100,
                    duration,
                    is_async,
                });
                let job3 = Box::new(DoubleJob {
                    data: i * 1000,
                    duration,
                    is_async,
                });

                // process job and print results.
                handles.push(job_queue.add_job(job1, DoubleJobPriority::Low)?.result_rx());
                handles.push(
                    job_queue
                        .add_job(job2, DoubleJobPriority::Medium)?
                        .result_rx(),
                );
                handles.push(
                    job_queue
                        .add_job(job3, DoubleJobPriority::High)?
                        .result_rx(),
                );
            }

            // wait for all jobs to complete.
            let mut results = futures::future::join_all(handles).await;

            // the results are in the same order as handles passed to join_all.
            // we sort them by the timestamp in job result, ascending.
            results.sort_by(
                |a_completion, b_completion| match (a_completion, b_completion) {
                    (Ok(JobCompletion::Finished(a_dyn)), Ok(JobCompletion::Finished(b_dyn))) => {
                        let a = a_dyn.as_any().downcast_ref::<DoubleJobResult>().unwrap().2;

                        let b = b_dyn.as_any().downcast_ref::<DoubleJobResult>().unwrap().2;

                        a.cmp(&b)
                    }
                    _ => panic!("at least one job did not finish"),
                },
            );

            // iterate job results and verify that:
            //   timestamp of each is greater than prev.
            //   input value of each is greater than prev, except every 9th item which should be < prev
            //     because there are nine jobs per level.
            let mut prev = Box::new(DoubleJobResult(9999, 0, start_of_test));
            for (i, c) in results.into_iter().enumerate() {
                let Ok(JobCompletion::Finished(dyn_result)) = c else {
                    panic!("A job did not finish");
                };

                let job_result = dyn_result.into_any().downcast::<DoubleJobResult>().unwrap();

                assert!(job_result.2 > prev.2);

                // we don't do the assertion for the 2nd job because the job-queue starts
                // processing immediately and so a race condition is setup where it is possible
                // for either the Low priority or High job to start processing first.
                if i != 1 {
                    assert!(job_result.0 < prev.0);
                }

                prev = job_result;
            }

            Ok(())
        }

        // this test demonstrates/verifies that a job can return a result back to
        // the job initiator.
        pub(super) async fn get_job_result(is_async: bool) -> anyhow::Result<()> {
            // create a job queue
            let job_queue = JobQueue::start();
            let duration = std::time::Duration::from_millis(20);

            // create 10 jobs
            for i in 0..10 {
                let job = Box::new(DoubleJob {
                    data: i,
                    duration,
                    is_async,
                });

                let result = job_queue
                    .add_job(job, DoubleJobPriority::Low)?
                    .result()
                    .await
                    .map_err(|e| e.into_sync())?;

                let job_result = result.into_any().downcast::<DoubleJobResult>().unwrap();

                assert_eq!(i, job_result.0);
                assert_eq!(i * 2, job_result.1);
            }

            Ok(())
        }

        // tests/demonstrates that a long running job can be cancelled early.
        pub(super) async fn cancel_job(is_async: bool) -> anyhow::Result<()> {
            // create a job queue
            let job_queue = JobQueue::start();
            // start a 1 hour job.
            let duration = std::time::Duration::from_secs(3600); // 1 hour job.

            let job = Box::new(DoubleJob {
                data: 10,
                duration,
                is_async,
            });
            let job_handle = job_queue.add_job(job, DoubleJobPriority::Low)?;

            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            let completion = job_handle.cancel_and_await().await.unwrap();
            assert!(matches!(completion, JobCompletion::Cancelled));

            Ok(())
        }

        // note: creates own tokio runtime.  caller must not use [tokio::test]
        //
        // this test starts a job that runs for 1 hour and then attempts to
        // shutdown tokio runtime via shutdown_timeout() with a 1 sec timeout.
        //
        // any async tasks should be aborted quickly.
        // any sync tasks will continue to run to completion.
        //
        // shutdown_timeout() will wait for tasks to abort for 1 sec and then
        // returns.  Any un-aborted tasks/threads become ignored/detached.
        // The OS can cleanup such threads when the process exits.
        //
        // the test checks that the shutdown completes in under 2 secs.
        //
        // the test demonstrates that shutdown_timeout() can be used to shutdown
        // tokio runtime even if sync (spawn_blocking) tasks/threads are still running
        // in the blocking threadpool.
        //
        // when called with is_async=true, it demonstrates that shutdown_timeout() also
        // aborts async jobs, as one would expect.
        pub(super) fn runtime_shutdown_timeout_force_cancels_job(
            is_async: bool,
        ) -> anyhow::Result<()> {
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(async {
                // create a job queue
                let job_queue = JobQueue::start();
                // start a 1 hour job.
                let duration = std::time::Duration::from_secs(3600); // 1 hour job.

                let job = Box::new(DoubleJob {
                    data: 10,
                    duration,
                    is_async,
                });
                let _rx = job_queue.add_job(job, DoubleJobPriority::Low)?;

                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                println!("finished scope");

                Ok(())
            });

            let start = std::time::Instant::now();

            println!("waiting 1 second for job before shutdown runtime");
            rt.shutdown_timeout(tokio::time::Duration::from_secs(1));

            assert!(start.elapsed() < std::time::Duration::from_secs(2));

            result
        }

        // note: creates own tokio runtime.  caller must not use [tokio::test]
        //
        // this test starts a job that runs for 5 secs and then attempts to
        // shutdown tokio runtime normally by dropping it.
        //
        // any async tasks should be aborted quickly.
        // any sync tasks will continue to run to completion.
        //
        // the tokio runtime does not complete the drop() until all tasks
        // have completed/aborted.
        //
        // the test checks that the job finishes in less than the 5 secs
        // required for full completion.  In other words, that it aborts.
        //
        // the test is expected to succeed for async jobs but fail for sync jobs.
        pub(super) fn runtime_shutdown_cancels_job(is_async: bool) -> anyhow::Result<()> {
            let rt = tokio::runtime::Runtime::new()?;
            let start = tokio::time::Instant::now();

            let result = rt.block_on(async {
                // create a job queue
                let job_queue = JobQueue::start();

                // this job takes at least 5 secs to complete.
                let duration = std::time::Duration::from_secs(5);

                let job = Box::new(DoubleJob {
                    data: 10,
                    duration,
                    is_async,
                });

                let rx_handle = job_queue.add_job(job, DoubleJobPriority::Low)?;
                drop(rx_handle);

                // sleep 50 ms to let job get started.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                Ok(())
            });

            // drop the tokio runtime. It will attempt to abort tasks.
            //   - async tasks can normally be aborted
            //   - spawn_blocking (sync) tasks cannot normally be aborted.
            drop(rt);

            // if test is successful, elapsed time should be less than the 5 secs
            // it takes for the job to complete.  (should be around 0.5 ms)

            // however it is expected/normal that sync tasks will not be aborted
            // and will run for full 5 secs.  thus this assert will fail for them.

            assert!(start.elapsed() < std::time::Duration::from_secs(5));

            result
        }

        // this test attempts to verify that the tasks spawned by the JobQueue
        // continue running until the JobQueue is dropped after the tokio
        // runtime is dropped.
        //
        // If the tasks are cencelled before JobQueue is dropped then a subsequent
        // api call that sends a msg will result in a "channel closed" error, which
        // is what the test checks for.
        //
        // note that the test has to do some tricky stuff to setup conditions
        // where the "channel closed" error can occur. It's a subtle issue.
        //
        // see description at:
        // https://github.com/tokio-rs/tokio/discussions/6961
        pub(super) fn spawned_tasks_live_as_long_as_jobqueue(is_async: bool) -> anyhow::Result<()> {
            let rt = tokio::runtime::Runtime::new()?;

            let result_ok: Arc<Mutex<bool>> = Arc::new(Mutex::new(true));

            let result_ok_clone = result_ok.clone();
            rt.block_on(async {
                // create a job queue
                let job_queue = Arc::new(JobQueue::start());

                // spawns background task that adds job
                let job_queue_cloned = job_queue.clone();
                let jh = tokio::spawn(async move {
                    // sleep 200 ms to let runtime finish.
                    // ie ensure drop(rt) will be reached and wait for us.
                    // note that we use std sleep.  if tokio sleep is used
                    // the test will always succeed due to the await point.
                    std::thread::sleep(std::time::Duration::from_millis(200));

                    let job = Box::new(DoubleJob {
                        data: 10,
                        duration: std::time::Duration::from_secs(1),
                        is_async,
                    });

                    let result = job_queue_cloned.add_job(job, DoubleJobPriority::Low);

                    // an assert on result.is_ok() would panic, but that panic would be
                    // printed and swallowed by tokio runtime, so the test would succeed
                    // despite the panic. instead we pass the result in a mutex so it
                    // can be asserted where it will be caught by the test runner.
                    *result_ok_clone.lock().unwrap() = result.is_ok();
                });

                // sleep 50 ms to let job get started.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                // note; awaiting the joinhandle makes the test succeed.

                jh.abort();
                let _ = jh.await;
            });

            // drop the tokio runtime. It will abort tasks.
            drop(rt);

            assert!(*result_ok.lock().unwrap());

            Ok(())
        }

        pub mod panics {
            use super::*;

            const PANIC_STR: &str = "job panics unexpectedly";

            struct PanicJob {
                is_async: bool,
            }

            #[async_trait::async_trait]
            impl Job for PanicJob {
                fn is_async(&self) -> bool {
                    self.is_async
                }

                fn run(&self, _cancel_rx: JobCancelReceiver) -> JobCompletion {
                    panic!("{}", PANIC_STR);
                }

                async fn run_async_cancellable(
                    &self,
                    _cancel_rx: JobCancelReceiver,
                ) -> JobCompletion {
                    panic!("{}", PANIC_STR);
                }
            }

            /// verifies that a job that panics will be ended properly.
            ///
            /// Properly means that:
            /// 1. an error is returned from job_handle.result() indicating job panicked.
            /// 2. caller is able to obtain panic info, which matches job's panic msg.
            /// 3. the job-queue continues accepting new jobs.
            /// 4. the job-queue continues processing jobs.
            ///
            /// async_job == true --> test an async job
            /// async_job == false --> test a blocking job
            pub async fn panic_in_job_ends_job_cleanly(async_job: bool) -> anyhow::Result<()> {
                // create a job queue
                let job_queue = JobQueue::start();

                let job = PanicJob {
                    is_async: async_job,
                };
                let job_handle = job_queue.add_job(Box::new(job), DoubleJobPriority::Low)?;

                let job_result = job_handle.result().await;

                println!("job_result: {:#?}", job_result);

                // verify that job_queue channel still open
                assert!(!job_queue.tx.is_closed());

                // verify that we get an error with the job's panic msg.
                assert!(matches!(
                    job_result.map_err(|e| e.into_sync()),
                    Err(JobHandleErrorSync::JobPanicked(e)) if e == *PANIC_STR
                ));

                // ensure we can still run another job afterwards.
                let newjob = Box::new(DoubleJob {
                    data: 10,
                    duration: std::time::Duration::from_millis(50),
                    is_async: false,
                });

                // ensure we can add another job.
                let new_job_handle = job_queue.add_job(newjob, DoubleJobPriority::Low)?;

                // ensure job processes and returns a result without error.
                assert!(new_job_handle.result().await.is_ok());

                Ok(())
            }
        }
    }
}

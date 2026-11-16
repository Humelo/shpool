// Copyright 2023 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/*! The ttl reaper is responsible to reaping sessions which
  have a ttl set. It uses a channel for a mailbox to listen
  for newly woken threads, remembers which session (by its
  inner state) registered each ttl so that it never clobbers
  a fresh session with the same name as a previous session,
  and uses a min heap to schedule wakeups in order to reap
  threads on time.
*/

use std::{
    cmp,
    collections::{BinaryHeap, HashMap},
    sync::{Arc, Weak},
    time::Instant,
};

use parking_lot::Mutex;
use tracing::{info, span, warn, Level};

use super::events;
use super::shell;

/// A request to reap a session once its ttl runs out.
pub struct Registration {
    pub session_name: String,
    /// The inner state of the session that registered the ttl. Only
    /// that session gets reaped: a kill followed by a new attach can
    /// put a different session under the same name before the ttl
    /// runs out. Weak so a dead session's state is not kept alive
    /// until then; it still pins the allocation, so the address
    /// cannot be reused by a later session.
    pub inner: Weak<Mutex<shell::SessionInner>>,
    pub reap_at: Instant,
}

/// Run the reaper thread loop. Should be invoked in a dedicated
/// thread.
pub fn run(
    new_sess: crossbeam_channel::Receiver<Registration>,
    shells: Arc<Mutex<HashMap<String, Box<shell::Session>>>>,
    events_bus: Arc<events::EventBus>,
) -> anyhow::Result<()> {
    let _s = span!(Level::INFO, "ttl_reaper").entered();

    let mut heap = BinaryHeap::new();

    loop {
        // empty heap loop, just waiting for new sessions to watch
        while heap.is_empty() {
            match new_sess.recv() {
                Ok(Registration { session_name, inner, reap_at }) => {
                    info!("scheduling first sess {} to be reaped at {:?}", &session_name, reap_at);
                    heap.push(Reapable { session_name, inner, reap_at });
                }
                Err(crossbeam_channel::RecvError) => {
                    info!("bailing due to RecvError in empty heap loop");
                    return Ok(());
                }
            }
        }

        while !heap.is_empty() {
            let wake_at = if let Some(reapable) = heap.peek() {
                reapable.reap_at
            } else {
                warn!("no reapable even with heap len {}, should be impossible", heap.len());
                continue;
            };

            crossbeam_channel::select! {
                recv(new_sess) -> new_sess_msg => {
                    match new_sess_msg {
                        Ok(Registration { session_name, inner, reap_at }) => {
                            info!("scheduling {} to be reaped at {:?}",
                                  &session_name, reap_at);
                            heap.push(Reapable {
                                session_name,
                                inner,
                                reap_at,
                            });
                        }
                        Err(crossbeam_channel::RecvError) => {
                            info!("bailing due to RecvError");
                            return Ok(())
                        },
                    }
                }
                recv(crossbeam_channel::at(wake_at)) -> _ => {
                    let reapable = heap.pop()
                        .expect("there to be an entry in a non-empty heap");
                    info!("waking up to reap {:?}", reapable);

                    let _s = span!(Level::INFO, "lock(shells)").entered();
                    let mut shells = shells.lock();
                    // Only reap the session that registered this ttl; a
                    // kill and a new attach may have put another session
                    // under the name since.
                    match shells.get(&reapable.session_name) {
                        Some(sess) if std::ptr::eq(
                            Arc::as_ptr(&sess.inner),
                            reapable.inner.as_ptr(),
                        ) => {
                            if let Err(e) = sess.kill() {
                                warn!("error trying to kill '{}': {:?}",
                                      reapable.session_name, e);
                            }
                        }
                        Some(_) => {
                            info!("'{}' was replaced by a newer session, leaving it in place",
                                  reapable.session_name);
                            continue;
                        }
                        None => {
                            warn!("tried to kill '{}' but it wasn't in the shells tab",
                                  reapable.session_name);
                            continue;
                        }
                    }
                    shells.remove(&reapable.session_name);
                    events_bus.publish(&events::Event::SessionRemoved);
                }
            }
        }
    }
}

/// A record in the min heap that we use to track the
/// sessions that need to be cleaned up.
#[derive(Debug)]
struct Reapable {
    session_name: String,
    inner: Weak<Mutex<shell::SessionInner>>,
    reap_at: Instant,
}

impl cmp::PartialEq for Reapable {
    fn eq(&self, rhs: &Reapable) -> bool {
        self.reap_at == rhs.reap_at
    }
}
impl cmp::Eq for Reapable {}

impl cmp::PartialOrd for Reapable {
    fn partial_cmp(&self, other: &Reapable) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl cmp::Ord for Reapable {
    fn cmp(&self, other: &Reapable) -> cmp::Ordering {
        // flip the ordering to make a min heap
        other.reap_at.cmp(&self.reap_at)
    }
}

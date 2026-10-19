"""Resolve a post_job reply to its real job id. Kept free of e2e.py's module-level fixture setup so
test_preparation.py can import it without starting a relay."""
import json
import time


def posted_job_id(get_job, posted, seconds=120, *, sleep=time.sleep, clock=time.monotonic, log=print):
    """The real job id of a post_job result. A slow post answers status=preparing with a
    preparation_id instead of job_id: poll get_job(handle) (as swarm.mjs does) until it reports
    status=posted. Never reposts. A get_job RuntimeError is a terminal preparation error; any
    status other than preparing/posted, a missing handle, or `seconds` without posting raises."""
    if posted.get('job_id'):
        return posted['job_id']
    handle = posted.get('preparation_id')
    if not handle:
        raise AssertionError(f'post_job returned neither job_id nor preparation_id: {json.dumps(posted)}')
    end, state = clock() + seconds, posted
    while clock() < end:
        try:
            state = get_job(handle)
        except RuntimeError as error:
            raise AssertionError(f'post_job preparation {handle} failed: {error}') from error
        if state.get('status') == 'posted' and state.get('job_id'):
            log(f'PASS post_job preparation {handle} posted as {state["job_id"]}', flush=True)
            return state['job_id']
        if state.get('status') != 'preparing':
            raise AssertionError(f'post_job preparation {handle}: unexpected get_job reply {json.dumps(state)}')
        sleep(1)
    raise AssertionError(f'post_job preparation {handle} still not posted after {seconds}s; last={json.dumps(state)}')

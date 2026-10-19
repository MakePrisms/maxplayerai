"""Unit tests for preparation.posted_job_id. Run: python3 -m unittest discover -s scripts/challenges -p 'test_*.py'"""
import unittest

from preparation import posted_job_id


class Clock:
    """A fake monotonic clock that only `sleep` advances, so the timeout case runs instantly."""

    def __init__(self):
        self.now = 0.0
        self.sleeps = 0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps += 1
        self.now += seconds


class FakeGetJob:
    """Replays scripted get_job replies (a RuntimeError instance is raised) and records handles."""

    def __init__(self, *replies):
        self.replies = list(replies)
        self.handles = []

    def __call__(self, handle):
        self.handles.append(handle)
        reply = self.replies.pop(0) if len(self.replies) > 1 else self.replies[0]
        if isinstance(reply, Exception):
            raise reply
        return reply


def run(get_job, posted, seconds=120):
    clock = Clock()
    result = posted_job_id(get_job, posted, seconds, sleep=clock.sleep, clock=clock, log=lambda *a, **k: None)
    return result, clock


class PostedJobId(unittest.TestCase):
    def test_direct_job_id_returns_without_polling(self):
        get_job = FakeGetJob({'status': 'posted', 'job_id': 'wrong'})
        result, _ = run(get_job, {'status': 'posted', 'job_id': 'direct-job'})
        self.assertEqual(result, 'direct-job')
        self.assertEqual(get_job.handles, [])

    def test_preparing_then_posted_polls_the_handle_until_the_real_job_id(self):
        get_job = FakeGetJob(
            {'status': 'preparing', 'preparation_id': 'preparation:p'},
            {'status': 'preparing', 'preparation_id': 'preparation:p'},
            {'status': 'posted', 'job_id': 'real-job', 'preparation_id': 'preparation:p'},
        )
        result, clock = run(get_job, {'status': 'preparing', 'preparation_id': 'preparation:p'})
        self.assertEqual(result, 'real-job')
        self.assertEqual(get_job.handles, ['preparation:p'] * 3)
        self.assertEqual(clock.sleeps, 2)

    def test_missing_ids_fail_clearly_without_polling(self):
        get_job = FakeGetJob({'status': 'posted', 'job_id': 'never'})
        for posted in ({}, {'status': 'preparing'}, {'job_id': '', 'preparation_id': ''}):
            with self.subTest(posted=posted), self.assertRaisesRegex(AssertionError, 'neither job_id nor preparation_id'):
                run(get_job, posted)
        self.assertEqual(get_job.handles, [])

    def test_terminal_get_job_error_fails_with_the_handle_and_cause(self):
        get_job = FakeGetJob(
            {'status': 'preparing', 'preparation_id': 'preparation:t'},
            RuntimeError('preparation failed: relay refused the offer'),
        )
        with self.assertRaisesRegex(AssertionError, r'preparation:t failed: .*relay refused the offer'):
            run(get_job, {'status': 'preparing', 'preparation_id': 'preparation:t'})
        self.assertEqual(len(get_job.handles), 2)

    def test_unexpected_status_fails_instead_of_polling_on(self):
        for reply in ({'status': 'failed', 'error': 'boom'}, {'status': 'posted'}, {}):
            get_job = FakeGetJob(reply)
            with self.subTest(reply=reply), self.assertRaisesRegex(AssertionError, 'unexpected get_job reply'):
                run(get_job, {'status': 'preparing', 'preparation_id': 'preparation:u'})
            self.assertEqual(get_job.handles, ['preparation:u'])

    def test_still_preparing_times_out_with_the_last_state(self):
        get_job = FakeGetJob({'status': 'preparing', 'preparation_id': 'preparation:slow', 'note': 'last'})
        with self.assertRaisesRegex(AssertionError, r'preparation:slow still not posted after 5s; last=.*"note": "last"'):
            run(get_job, {'status': 'preparing', 'preparation_id': 'preparation:slow'}, seconds=5)
        self.assertEqual(len(get_job.handles), 5)


if __name__ == '__main__':
    unittest.main()

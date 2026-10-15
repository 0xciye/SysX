from receiver_backend.serial_command_queue import QueuedSerialCommand, SerialCommandQueueConsumer


class DeferredWriter:
    def submit_write(self, operation_id, command, buffer, completed):
        self.pending = (operation_id, command, buffer, completed)
        return True


def test_type_zero_write_retains_payload_until_async_completion():
    writer = DeferredWriter()
    completed = []
    command = QueuedSerialCommand(0, b"km.move(4,-2)\r\n", 25, object())
    consumer = SerialCommandQueueConsumer(writer, lambda *args: completed.append(args))

    assert consumer.consume_one(lambda: command)
    operation_id, submitted, buffer, finish = writer.pending
    assert operation_id == 1
    assert submitted is command
    assert buffer == command.bytes
    assert consumer.in_flight_count == 1
    assert completed == []

    finish(True, None)
    finish(False, "duplicate completion")

    assert consumer.in_flight_count == 0
    assert completed == [(command, True, None)]


def test_empty_and_nonzero_kind_requests_do_not_submit_type_zero_writes():
    writer = DeferredWriter()
    consumer = SerialCommandQueueConsumer(writer)

    assert not consumer.consume_one(lambda: QueuedSerialCommand(0, b""))
    assert not consumer.consume_one(lambda: QueuedSerialCommand(1, b"timer"))
    assert not consumer.consume_one(lambda: None)
    assert not hasattr(writer, "pending")
    assert consumer.in_flight_count == 0


def test_failed_submission_releases_in_flight_buffer_and_reports_error():
    class RejectingWriter:
        def submit_write(self, operation_id, command, buffer, completed):
            return False

    command = QueuedSerialCommand(0, b"km.move(1,1)\r\n")
    completed = []
    consumer = SerialCommandQueueConsumer(RejectingWriter(), lambda *args: completed.append(args))

    assert not consumer.consume_one(lambda: command)
    assert consumer.in_flight_count == 0
    assert completed == [(command, False, "serial write submission failed")]

"""Questions are data, not messages: only an explicit answer settles one."""
from dataclasses import dataclass, field

QUESTION_BYTES_MAX = 65536
QUESTION_CHOICES_MAX = 32
QUESTIONS_MAX = 32

@dataclass
class Choice:
    """An offered label and optional description. Labels must be nonempty and unique."""
    label: str
    description: str = ""

@dataclass
class QuestionReference:
    """An inert file reference accompanying a question, never an instruction to open a file.

    `path` is a nonempty path as the asker sees it. `line` and `end_line` are optional
    one-based positions; an end requires a start and must not precede it. `description`
    explains why the file matters. Owners display these as untrusted text, not markup.
    """
    path: str
    line: int | None = None
    end_line: int | None = None
    description: str = ""


@dataclass
class QuestionContext:
    """Optional background for a short question prompt: longer prose, file references, or both.

    `description` is plain text, not trusted markup. A string in `references` becomes a
    QuestionReference(path). Context is data under the same total question-size bound.
    """
    description: str = ""
    references: list[QuestionReference] = field(default_factory=list)

    def __post_init__(self):
        self.references = [QuestionReference(r) if type(r) is str else r for r in self.references]

    def _validate(self):
        if type(self.description) is not str:
            raise TypeError("QuestionContext.description must be str")
        for ref in self.references:
            if type(ref) is not QuestionReference or type(ref.path) is not str or not ref.path.strip():
                raise ValueError("a question reference needs a nonempty path")
            if type(ref.description) is not str:
                raise TypeError("QuestionReference.description must be str")
            if ref.line is not None and (type(ref.line) is not int or ref.line < 1):
                raise ValueError("QuestionReference.line must be a positive one-based integer")
            if ref.end_line is not None and (type(ref.end_line) is not int or
                    ref.line is None or ref.end_line < ref.line):
                raise ValueError("QuestionReference.end_line requires a start and must not precede it")


@dataclass
class Question:
    """A decision for your parent (the session owner for a primary), asked with `runtime.ask`.

    Empty choices means free-form. Otherwise `multiple=False` permits one choice and
    `multiple=True` permits zero or more; `other=False` forbids additional text.
    Strings in choices become Choice(label). At ask, the limits are 32 choices,
    65,536 UTF-8 JSON bytes, and 32 open questions per asker, separate from requests-max.
    `context` optionally carries a QuestionContext: longer background text and file references,
    included in the same size bound. There is no answer timeout: cancel a question you give
    up on. A child asks its parent; a primary asks the session owner.
    """
    text: str
    choices: list[Choice] = field(default_factory=list)
    multiple: bool = False
    other: bool = True
    context: QuestionContext | None = None

    def __post_init__(self):
        self.choices = [Choice(c) if type(c) is str else c for c in self.choices]

    def _validate(self):
        import json
        from dataclasses import asdict
        if type(self.text) is not str or not self.text.strip():
            raise ValueError("a Question needs nonempty text")
        if type(self.multiple) is not bool or type(self.other) is not bool:
            raise TypeError("Question.multiple and other must be bool")
        if len(self.choices) > QUESTION_CHOICES_MAX:
            raise ValueError(f"question exceeds choices limit {QUESTION_CHOICES_MAX}")
        labels = []
        for c in self.choices:
            if type(c) is not Choice or type(c.label) is not str or not c.label.strip():
                raise ValueError("each choice needs a nonempty label")
            if type(c.description) is not str:
                raise TypeError("Choice.description must be str")
            if c.label in labels:
                raise ValueError(f"duplicate choice label: {c.label!r}")
            labels.append(c.label)
        if self.context is not None:
            if type(self.context) is not QuestionContext:
                raise TypeError("Question.context must be a QuestionContext or None")
            self.context._validate()
        if len(json.dumps(asdict(self), ensure_ascii=False).encode()) > QUESTION_BYTES_MAX:
            raise ValueError(f"question exceeds size limit {QUESTION_BYTES_MAX} bytes")

@dataclass
class Answer:
    """An explicit answer: choice labels, optional text, answerer, and attribution chain.

    Choices are returned in the question's order. A host program passes one to
    `await question.answer(answer)`; invalid answers leave the question open.
    """
    choices: list[str] = field(default_factory=list)
    text: str | None = None
    by: str = "application"
    chain: list[str] = field(default_factory=list)


def _answer(question, answer):
    """Validate and normalize an explicit answer without changing the question."""
    import copy
    if type(answer) is not Answer:
        raise TypeError("answer must be an outrig.Answer")
    if (type(answer.choices) is not list or any(type(c) is not str for c in answer.choices)
            or (answer.text is not None and type(answer.text) is not str)):
        raise TypeError("Answer needs string choices and optional string text")
    labels = [c.label for c in question.choices]
    if len(set(answer.choices)) != len(answer.choices) or any(c not in labels for c in answer.choices):
        raise ValueError("unknown or duplicate choice")
    if not question.multiple and len(answer.choices) > 1:
        raise ValueError("a single-choice question takes at most one choice")
    if question.choices and not question.other and answer.text is not None:
        raise ValueError("this question does not accept free text (other=False)")
    if ((not question.multiple or not question.choices) and not answer.choices
            and (answer.text is None or not answer.text.strip())):
        raise ValueError("an answer needs a choice or text")
    answer = copy.deepcopy(answer)
    answer.choices = [c for c in labels if c in answer.choices]
    return answer


import asyncio
from ._agents import Handle


class _QuestionFuture(asyncio.Future):
    """A cancellation-isolated view. Only await/result consumes the notice, not exception()."""
    def __init__(self, work):
        super().__init__(loop=work.future.get_loop())
        self._work = work

        def transfer(source):
            self._copy_to(source, self)

        if work.future.done():
            transfer(work.future)
        else:
            work.future.add_done_callback(transfer)

    @staticmethod
    def _copy_to(source, target):
        if target.done():
            return
        if asyncio.Future.cancelled(source):
            target.cancel()
            return
        error = asyncio.Future.exception(source)
        if error is None:
            target.set_result(asyncio.Future.result(source))
        else:
            target.set_exception(error)
            # This is an internal transfer, not a read by the asker's code.
            asyncio.Future.exception(target)

    def __await__(self):
        # Tasks call result() while waking, even if they will inject a racing
        # cancellation instead of returning the value. Yield a plain future so
        # only the continuation below, not that internal read, observes it.
        waiting = asyncio.Future(loop=self.get_loop())
        def transfer(source):
            self._copy_to(source, waiting)
        if self.done():
            transfer(self)
        else:
            self.add_done_callback(transfer)
        try:
            value = yield from waiting.__await__()
        except asyncio.CancelledError:
            raise
        except BaseException:
            self._work.observed()
            raise
        else:
            self._work.observed()
            return value
        finally:
            self.remove_done_callback(transfer)

    def result(self):
        try:
            value = super().result()
        except (asyncio.InvalidStateError, asyncio.CancelledError):
            raise
        except BaseException:
            self._work.observed()
            raise
        self._work.observed()
        return value


class QuestionHandle(Handle):
    """A question's request handle; await/result consumes its wake notice."""
    def __await__(self):
        return _QuestionFuture(self._work).__await__()

    @property
    def future(self):
        """A cancellation-isolated view; await/result acknowledges, exception inspection does not."""
        future = self._future
        if future is None or future.cancelled():
            future = self._future = _QuestionFuture(self._work)
        return future

    def result(self):
        try:
            value = super().result()
        except asyncio.InvalidStateError:
            raise
        except BaseException:
            self._work.observed()
            raise
        self._work.observed()
        return value

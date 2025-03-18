FROM python:3.12-slim

COPY requirements.txt .

RUN pip install -r requirements.txt

COPY src /app/src

CMD ["kopf", "run", "/app/src/operator.py", "--verbose"]

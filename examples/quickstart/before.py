from openai import OpenAI
from dotenv import load_dotenv

load_dotenv()

with OpenAI() as client:
    response = client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[
            {
                "role": "user",
                "content": "Write a haiku about artificial intelligence.",
            }
        ],
    )

print(response)

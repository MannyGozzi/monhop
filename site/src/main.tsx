import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { RouterProvider } from '@tanstack/react-router'

import './index.css'
import { router } from './router'

const container = document.querySelector('#root')
if (!container) throw new Error('Missing #root container')

createRoot(container).render(
  <StrictMode>
    <RouterProvider router={router} />
  </StrictMode>,
)

import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import JSONViewer from './App.tsx'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <JSONViewer />
  </StrictMode>,
)
